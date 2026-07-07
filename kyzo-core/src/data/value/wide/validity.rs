/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! `Validity`: the time-axis value kind and its coordinate vocabulary,
//! the law **imported** from the sealed bitemporal guardrail
//! (`data/bitemporal.rs` + the validity-in-key storage contract), never
//! invented here:
//!
//! - A validity is a pure value: (timestamp in microseconds, assert
//!   flag). No ambient clock, no query-time normalization; the wall
//!   clock lives in the runtime tier only.
//! - Total order is **as-of seek order**: descending timestamp (the
//!   latest instant sorts first), assert before retract at the same
//!   instant. The order is declared **by shape** — `Reverse` sits in the
//!   fields, so the derived `Ord` IS the imported law and cannot be
//!   misread as chronological: `later < earlier` is what the type says
//!   on its face.
//! - Claim polarity beyond the flag (assert/retract/erase) rides in row
//!   VALUES per the guardrail, not in this kind.
//!
//! Canonical payload (format v1): the 8-byte DESCENDING timestamp key
//! (ascending sign-flipped big-endian, complemented), then one polarity
//! byte (`0x00` assert, `0x01` retract; anything else refuses). Note:
//! this kind treats every `i64` — including `i64::MAX` — as an ordinary
//! instant; [`TERMINAL_VALIDITY`] below is a slot value (the maximum
//! slot ENCODING), not a magic timestamp.

use std::cmp::Reverse;

/// A valid-time coordinate in as-of seek order: `Reverse<i64>` of the
/// microsecond timestamp, so smaller means LATER — exactly how the
/// stored key slots sort.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ValidityTs(pub Reverse<i64>);

impl ValidityTs {
    pub fn from_raw(ts_micros: i64) -> ValidityTs {
        ValidityTs(Reverse(ts_micros))
    }

    pub fn raw(self) -> i64 {
        self.0.0
    }
}

/// The latest representable coordinate (sorts FIRST in seek order).
pub const MAX_VALIDITY_TS: ValidityTs = ValidityTs(Reverse(i64::MAX));

/// The validity value: a valid-time instant and its assert flag, in
/// as-of seek order by shape (see the module docs).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Validity {
    pub timestamp: ValidityTs,
    pub is_assert: Reverse<bool>,
}

/// The maximum slot ENCODING: sorts after every other validity slot
/// (oldest representable instant, retract flag) — the seek target that
/// lands past a fact's entire history.
pub const TERMINAL_VALIDITY: Validity = Validity {
    timestamp: ValidityTs(Reverse(i64::MIN)),
    is_assert: Reverse(false),
};

impl Validity {
    pub const fn new(ts_micros: i64, is_assert: bool) -> Validity {
        Validity {
            timestamp: ValidityTs(Reverse(ts_micros)),
            is_assert: Reverse(is_assert),
        }
    }

    pub fn ts_micros(self) -> i64 {
        self.timestamp.0.0
    }

    pub fn is_assert(self) -> bool {
        self.is_assert.0
    }

    /// The imported as-of order — an alias for the derived `Ord`, kept
    /// as a named authority for call sites that want to say what they
    /// mean.
    pub fn cmp_as_of_order(self, other: Validity) -> std::cmp::Ordering {
        self.cmp(&other)
    }
}

/// A stored key-slot builder: slot flags are PINNED to assert (polarity
/// lives in row values, per the guardrail), so a slot is fully
/// determined by its coordinate.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StoredValiditySlot(pub ValidityTs);

impl StoredValiditySlot {
    pub fn new(ts: ValidityTs) -> StoredValiditySlot {
        StoredValiditySlot(ts)
    }

    pub fn as_validity(self) -> Validity {
        Validity {
            timestamp: self.0,
            is_assert: Reverse(true),
        }
    }
}

/// The bitemporal as-of coordinate pair: what instant of the world
/// (`valid`) as recorded by what instant of the record (`sys`). A pure
/// value — no clock in the plane; "now" is the runtime tier's word.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct AsOf {
    pub valid: ValidityTs,
    pub sys: ValidityTs,
}

impl AsOf {
    /// The currently-recorded view of the world at `valid`: system time
    /// pinned to the latest coordinate.
    pub fn current(valid: ValidityTs) -> AsOf {
        AsOf {
            valid,
            sys: MAX_VALIDITY_TS,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imported_law_descending_ts_assert_first_by_shape() {
        // Later instants sort first (descending), assert before retract —
        // now the DERIVED order, declared by the Reverse fields.
        assert!(Validity::new(10, true) < Validity::new(5, true));
        assert!(Validity::new(5, true) < Validity::new(5, false));
        assert!(Validity::new(i64::MAX, false) < Validity::new(i64::MIN, true));
        assert_eq!(Validity::new(7, true), Validity::new(7, true));
        // The named alias agrees with the shape.
        assert_eq!(
            Validity::new(10, true).cmp_as_of_order(Validity::new(5, true)),
            std::cmp::Ordering::Less
        );
    }

    #[test]
    fn coordinates_and_slots_speak_seek_order() {
        // Smaller ValidityTs means later.
        assert!(ValidityTs::from_raw(100) < ValidityTs::from_raw(5));
        assert!(MAX_VALIDITY_TS < ValidityTs::from_raw(0));
        assert_eq!(ValidityTs::from_raw(42).raw(), 42);
        // Terminal sorts after every ordinary slot.
        assert!(Validity::new(i64::MIN, true) < TERMINAL_VALIDITY);
        assert!(Validity::new(i64::MAX, false) < TERMINAL_VALIDITY);
        // Stored slots are pinned to assert.
        let slot = StoredValiditySlot::new(ValidityTs::from_raw(7)).as_validity();
        assert!(slot.is_assert());
        assert_eq!(slot.ts_micros(), 7);
        // AsOf::current pins system time to the latest coordinate.
        let a = AsOf::current(ValidityTs::from_raw(9));
        assert_eq!(a.sys, MAX_VALIDITY_TS);
        assert_eq!(a.valid.raw(), 9);
    }
}
