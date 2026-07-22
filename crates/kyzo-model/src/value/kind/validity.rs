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
//!
//! Construction doors (P001–P004 / P002):
//! - [`ValidityTs::for_assertion`] — user write coordinate; refuses the
//!   reserved terminal tick.
//! - [`ValidityTs::from_raw`] — crate storage-decode / seek coordinate.
//! - [`Validity::new`] — sole [`Validity`] mint; refuses assert+reserved.
//! - [`ValiditySlot::from_stored`] — wire/seek payload; assert+reserved is
//!   a [`ValiditySeekBound`], never a [`Validity`].
//! - [`AsOf::current`] / [`AsOf::at`] — only as-of pair mints.
//! - [`StoredValiditySlot::new`] — only stored-slot mint.

use std::cmp::Reverse;

/// A valid-time coordinate in as-of seek order: `Reverse<i64>` of the
/// microsecond timestamp, so smaller means LATER — exactly how the
/// stored key slots sort.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(transparent)]
pub struct ValidityTs(Reverse<i64>);

const _: () = assert!(std::mem::size_of::<ValidityTs>() == std::mem::size_of::<Reverse<i64>>());
const _: () = assert!(std::mem::align_of::<ValidityTs>() == std::mem::align_of::<Reverse<i64>>());

impl ValidityTs {
    /// Storage-decode / seek-coordinate door: any `i64` instant, including
    /// the reserved terminal. Not a user-assertion path — that is
    /// [`for_assertion`].
    pub fn from_raw(ts_micros: i64) -> ValidityTs {
        ValidityTs(Reverse(ts_micros))
    }

    pub fn raw(self) -> i64 {
        self.0.0
    }
}

impl serde::Serialize for ValidityTs {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_i64(self.raw())
    }
}

impl<'de> serde::Deserialize<'de> for ValidityTs {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        Ok(ValidityTs::from_raw(
            <i64 as serde::Deserialize>::deserialize(deserializer)?,
        ))
    }
}

impl ValidityTs {
    /// The user-assertion door: a user-asserted write validity can never
    /// be the reserved terminal tick (`i64::MAX` / `'END'`), the instant
    /// every open-end sentinel and derived interval reads as "still open".
    /// `None` refuses it; diagnostics (spans) are the caller's business.
    pub fn for_assertion(ts_micros: i64) -> Option<ValidityTs> {
        if ts_micros == i64::MAX {
            None
        } else {
            Some(ValidityTs::from_raw(ts_micros))
        }
    }
}

/// The latest representable coordinate (sorts FIRST in seek order).
pub const MAX_VALIDITY_TS: ValidityTs = ValidityTs(Reverse(i64::MAX));

/// The validity value: a valid-time instant and its assert flag, in
/// as-of seek order by shape (see the module docs).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Validity {
    timestamp: ValidityTs,
    is_assert: Reverse<bool>,
}

/// The maximum slot ENCODING: sorts after every other validity slot
/// (oldest representable instant, retract flag) — the seek target that
/// lands past a fact's entire history.
pub const TERMINAL_VALIDITY: Validity = Validity {
    timestamp: ValidityTs(Reverse(i64::MIN)),
    is_assert: Reverse(false),
};

impl Validity {
    /// Sole [`Validity`] mint. Refuses assert of the reserved terminal
    /// tick (`i64::MAX`) — that state is unrepresentable as [`Validity`].
    /// Wire/seek open-end bounds use [`ValiditySlot::from_stored`] /
    /// [`ValiditySeekBound`].
    pub fn new(timestamp: ValidityTs, is_assert: bool) -> Option<Validity> {
        if is_assert && timestamp.raw() == i64::MAX {
            None
        } else {
            Some(Validity {
                timestamp,
                is_assert: Reverse(is_assert),
            })
        }
    }

    pub fn ts_micros(self) -> i64 {
        self.timestamp.0.0
    }

    /// The valid-time coordinate as its proven type.
    pub fn timestamp(self) -> ValidityTs {
        self.timestamp
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

/// Sealed seek/slot bound that may hold assert+`i64::MAX`. Not a
/// [`Validity`] — that type cannot represent open-end assert.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ValiditySeekBound {
    timestamp: ValidityTs,
    is_assert: Reverse<bool>,
}

impl ValiditySeekBound {
    pub(crate) fn new(timestamp: ValidityTs, is_assert: bool) -> ValiditySeekBound {
        ValiditySeekBound {
            timestamp,
            is_assert: Reverse(is_assert),
        }
    }

    pub fn timestamp(self) -> ValidityTs {
        self.timestamp
    }

    pub fn ts_micros(self) -> i64 {
        self.timestamp.raw()
    }

    pub fn is_assert(self) -> bool {
        self.is_assert.0
    }

    /// Lift to [`Validity`] when the bound is a representable value.
    pub fn try_into_validity(self) -> Option<Validity> {
        Validity::new(self.timestamp, self.is_assert())
    }
}

/// Tag::Validity wire/key payload: a proven [`Validity`], or a sealed
/// [`ValiditySeekBound`] when the open-end assert terminal is required.
/// Assert+`i64::MAX` is never a [`Validity`].
///
/// Order and hash are the as-of payload `(timestamp, is_assert)` only —
/// never the Value/Seek discriminant. Discriminant-first `Ord` would
/// disagree with the canonical bytes (the one law).
#[derive(Clone, Copy, Debug)]
pub enum ValiditySlot {
    Value(Validity),
    Seek(ValiditySeekBound),
}

impl PartialEq for ValiditySlot {
    fn eq(&self, other: &Self) -> bool {
        self.timestamp() == other.timestamp() && self.is_assert() == other.is_assert()
    }
}

impl Eq for ValiditySlot {}

impl PartialOrd for ValiditySlot {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ValiditySlot {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.timestamp()
            .cmp(&other.timestamp())
            .then_with(|| Reverse(self.is_assert()).cmp(&Reverse(other.is_assert())))
    }
}

impl std::hash::Hash for ValiditySlot {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.timestamp().hash(state);
        self.is_assert().hash(state);
    }
}

impl ValiditySlot {
    /// Storage-decode / seek-slot door. Assert of the reserved terminal
    /// becomes [`ValiditySeekBound`]; every other pair is [`Validity`].
    pub fn from_stored(timestamp: ValidityTs, is_assert: bool) -> ValiditySlot {
        match Validity::new(timestamp, is_assert) {
            Some(v) => ValiditySlot::Value(v),
            None => ValiditySlot::Seek(ValiditySeekBound::new(timestamp, is_assert)),
        }
    }

    pub fn timestamp(self) -> ValidityTs {
        match self {
            ValiditySlot::Value(v) => v.timestamp(),
            ValiditySlot::Seek(s) => s.timestamp(),
        }
    }

    pub fn ts_micros(self) -> i64 {
        self.timestamp().raw()
    }

    pub fn is_assert(self) -> bool {
        match self {
            ValiditySlot::Value(v) => v.is_assert(),
            ValiditySlot::Seek(s) => s.is_assert(),
        }
    }

    pub fn cmp_as_of_order(self, other: ValiditySlot) -> std::cmp::Ordering {
        self.cmp(&other)
    }

    /// Proven value only — `None` for a sealed open-end seek bound.
    pub fn as_validity(self) -> Option<Validity> {
        match self {
            ValiditySlot::Value(v) => Some(v),
            ValiditySlot::Seek(_) => None,
        }
    }
}

impl From<Validity> for ValiditySlot {
    fn from(v: Validity) -> ValiditySlot {
        ValiditySlot::Value(v)
    }
}

/// A stored key-slot builder: slot flags are PINNED to assert (polarity
/// lives in row values, per the guardrail), so a slot is fully
/// determined by its coordinate.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(transparent)]
pub struct StoredValiditySlot(ValidityTs);

const _: () =
    assert!(std::mem::size_of::<StoredValiditySlot>() == std::mem::size_of::<ValidityTs>());
const _: () =
    assert!(std::mem::align_of::<StoredValiditySlot>() == std::mem::align_of::<ValidityTs>());

impl StoredValiditySlot {
    pub fn new(ts: ValidityTs) -> StoredValiditySlot {
        StoredValiditySlot(ts)
    }

    pub fn as_datavalue(self) -> super::super::DataValue {
        super::super::DataValue::Validity(self.as_slot())
    }

    /// Slot as wire payload — assert+terminal is [`ValiditySeekBound`].
    pub fn as_slot(self) -> ValiditySlot {
        ValiditySlot::from_stored(self.0, true)
    }

    /// Proven [`Validity`] when the slot is not the open-end assert bound.
    pub fn as_validity(self) -> Option<Validity> {
        self.as_slot().as_validity()
    }
}

/// The bitemporal as-of coordinate pair: what instant of the world
/// (`valid`) as recorded by what instant of the record (`sys`). A pure
/// value — no clock in the plane; "now" is the runtime tier's word.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct AsOf {
    valid: ValidityTs,
    sys: ValidityTs,
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

    /// The general two-coordinate historical read: what the record said at
    /// system time `sys` about valid time `valid`. Argument order is
    /// `(sys, valid)` — system coordinate first — matching the field
    /// documentation; [`AsOf::current`] is the special case `sys` pinned
    /// to the latest coordinate.
    pub fn at(sys: ValidityTs, valid: ValidityTs) -> AsOf {
        AsOf { valid, sys }
    }

    pub fn valid(self) -> ValidityTs {
        self.valid
    }

    pub fn sys(self) -> ValidityTs {
        self.sys
    }
}

impl serde::Serialize for AsOf {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("AsOf", 2)?;
        state.serialize_field("valid", &self.valid)?;
        state.serialize_field("sys", &self.sys)?;
        state.end()
    }
}

impl<'de> serde::Deserialize<'de> for AsOf {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        #[derive(serde_derive::Deserialize)]
        struct AsOfDe {
            valid: ValidityTs,
            sys: ValidityTs,
        }
        let AsOfDe { valid, sys } = <AsOfDe as serde::Deserialize>::deserialize(deserializer)?;
        Ok(AsOf { valid, sys })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(ts: i64, is_assert: bool) -> Validity {
        Validity::new(ValidityTs::from_raw(ts), is_assert).expect("representable Validity")
    }

    fn slot(ts: i64, is_assert: bool) -> ValiditySlot {
        ValiditySlot::from_stored(ValidityTs::from_raw(ts), is_assert)
    }

    #[test]
    fn imported_law_descending_ts_assert_first_by_shape() {
        // Later instants sort first (descending), assert before retract —
        // now the DERIVED order, declared by the Reverse fields.
        assert!(v(10, true) < v(5, true));
        assert!(v(5, true) < v(5, false));
        assert!(v(i64::MAX, false) < v(i64::MIN, true));
        assert_eq!(v(7, true), v(7, true));
        // The named alias agrees with the shape.
        assert_eq!(
            v(10, true).cmp_as_of_order(v(5, true)),
            std::cmp::Ordering::Less
        );
        // Open-end assert lives only on ValiditySlot/SeekBound.
        assert!(matches!(slot(i64::MAX, true), ValiditySlot::Seek(_)));
        assert!(slot(i64::MAX, true).as_validity().is_none());
    }

    #[test]
    fn coordinates_and_slots_speak_seek_order() {
        // Smaller ValidityTs means later.
        assert!(ValidityTs::from_raw(100) < ValidityTs::from_raw(5));
        assert!(MAX_VALIDITY_TS < ValidityTs::from_raw(0));
        assert_eq!(ValidityTs::from_raw(42).raw(), 42);
        // Terminal sorts after every ordinary slot.
        assert!(v(i64::MIN, true) < TERMINAL_VALIDITY);
        assert!(v(i64::MAX, false) < TERMINAL_VALIDITY);
        // Stored slots are pinned to assert; ordinary slots are Validity.
        let ordinary = StoredValiditySlot::new(ValidityTs::from_raw(7)).as_validity();
        assert_eq!(ordinary.map(|s| s.ts_micros()), Some(7));
        assert!(ordinary.is_some_and(|s| s.is_assert()));
        // Open-end assert slot is SeekBound, never Validity.
        let open = StoredValiditySlot::new(MAX_VALIDITY_TS).as_slot();
        assert!(matches!(open, ValiditySlot::Seek(_)));
        assert!(open.as_validity().is_none());
        // The user-assertion door refuses exactly the terminal tick.
        assert!(ValidityTs::for_assertion(i64::MAX).is_none());
        assert_eq!(ValidityTs::for_assertion(0), Some(ValidityTs::from_raw(0)));
        // Assert of the reserved terminal is unrepresentable as Validity.
        assert!(Validity::new(MAX_VALIDITY_TS, true).is_none());
        assert!(Validity::new(MAX_VALIDITY_TS, false).is_some());
        // AsOf::current pins system time to the latest coordinate.
        let a = AsOf::current(ValidityTs::from_raw(9));
        assert_eq!(a.sys(), MAX_VALIDITY_TS);
        assert_eq!(a.valid().raw(), 9);
    }
}
