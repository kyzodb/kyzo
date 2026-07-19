/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Reference temporal vocabulary: AsOf, Event, resolve*, intervals, diff, compose.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use kyzo_model::value::Tuple;

/// A bitemporal read coordinate in plain ascending `i64` (larger means later).
///
/// Exact correspondence to the Reverse-wrapped real type: wrap each field
/// in `ValidityTs::from_raw(_)` and mint through the real type's `at` /
/// `current` doors. Ascending `t <= v` here is the real type's descending
/// `ValidityTs(t) >= ValidityTs(v)`.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AsOf {
    pub valid: i64,
    pub sys: i64,
}

impl AsOf {
    pub const fn current() -> Self {
        AsOf {
            valid: i64::MAX,
            sys: i64::MAX,
        }
    }

    pub const fn current_at(valid: i64) -> Self {
        AsOf {
            valid,
            sys: i64::MAX,
        }
    }
}

/// One stored point-event in a fact's bitemporal history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Assert {
        key: Tuple,
        payload: Tuple,
        valid: i64,
        sys: i64,
    },
    Retract {
        key: Tuple,
        valid: i64,
        sys: i64,
    },
    Erase {
        key: Tuple,
        valid: i64,
        sys: i64,
    },
}

/// Named refusal when an event claims the reserved `@ 'END'` valid instant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReservedValidInstant;

impl fmt::Display for ReservedValidInstant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(
            "valid instant i64::MAX is reserved for the `@ 'END'` write-side \
             sentinel; no event may claim it as its own coordinate",
        )
    }
}

impl std::error::Error for ReservedValidInstant {}

impl Event {
    fn check_valid_not_reserved(valid: i64) -> Result<(), ReservedValidInstant> {
        if valid == i64::MAX {
            return Err(ReservedValidInstant);
        }
        Ok(())
    }

    pub fn assert(
        key: Tuple,
        payload: Tuple,
        valid: i64,
        sys: i64,
    ) -> Result<Self, ReservedValidInstant> {
        Self::check_valid_not_reserved(valid)?;
        Ok(Event::Assert {
            key,
            payload,
            valid,
            sys,
        })
    }

    pub fn retract(key: Tuple, valid: i64, sys: i64) -> Result<Self, ReservedValidInstant> {
        Self::check_valid_not_reserved(valid)?;
        Ok(Event::Retract { key, valid, sys })
    }

    pub fn erase(key: Tuple, valid: i64, sys: i64) -> Result<Self, ReservedValidInstant> {
        Self::check_valid_not_reserved(valid)?;
        Ok(Event::Erase { key, valid, sys })
    }

    /// Untimed embedding: assert at canonical instant `(valid = 0, sys = 0)`.
    pub fn untimed(tuple: Tuple) -> Self {
        Event::Assert {
            key: tuple,
            payload: Tuple::new(),
            valid: 0,
            sys: 0,
        }
    }

    pub fn key(&self) -> &Tuple {
        match self {
            Event::Assert { key, .. } | Event::Retract { key, .. } | Event::Erase { key, .. } => {
                key
            }
        }
    }

    pub fn valid(&self) -> i64 {
        match self {
            Event::Assert { valid, .. }
            | Event::Retract { valid, .. }
            | Event::Erase { valid, .. } => *valid,
        }
    }

    pub fn sys(&self) -> i64 {
        match self {
            Event::Assert { sys, .. } | Event::Retract { sys, .. } | Event::Erase { sys, .. } => {
                *sys
            }
        }
    }

    pub fn payload(&self) -> Option<&Tuple> {
        match self {
            Event::Assert { payload, .. } => Some(payload),
            Event::Retract { .. } | Event::Erase { .. } => None,
        }
    }
}

/// Local polarity vocabulary for temporal tests / Event mapping (not the
/// body-literal [`crate::eval::Polarity`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaimPolarity {
    Assert,
    Retract,
    Erase,
}

/// The governing tuple for one fact — all events sharing a key — at `at`.
pub fn resolve_events(events: &[&Event], at: AsOf) -> Option<Tuple> {
    let mut instants: Vec<i64> = events
        .iter()
        .map(|e| e.valid())
        .filter(|v| *v <= at.valid)
        .collect();
    instants.sort_unstable();
    instants.dedup();
    for instant in instants.into_iter().rev() {
        let governing = events
            .iter()
            .filter(|e| e.valid() == instant && e.sys() <= at.sys)
            .max_by_key(|e| e.sys());
        match governing {
            Some(Event::Assert { key, payload, .. }) => {
                let mut tuple = key.clone();
                tuple.extend(payload.iter().cloned());
                return Some(tuple);
            }
            Some(Event::Retract { .. }) => return None,
            Some(Event::Erase { .. }) | None => {}
        }
    }
    None
}

pub fn resolve(history: &[Event], key: &Tuple, at: AsOf) -> Option<Tuple> {
    let events: Vec<&Event> = history.iter().filter(|e| e.key() == key).collect();
    resolve_events(&events, at)
}

pub fn resolve_relation(history: &[Event], at: AsOf) -> BTreeSet<Tuple> {
    let mut by_key: BTreeMap<&Tuple, Vec<&Event>> = BTreeMap::new();
    for e in history {
        by_key.entry(e.key()).or_default().push(e);
    }
    by_key
        .into_values()
        .filter_map(|events| resolve_events(&events, at))
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    Valid,
    Sys,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Interval {
    pub start: i64,
    pub end: i64,
    pub tuple: Tuple,
}

pub const OPEN_END: i64 = i64::MAX;

pub fn derive_intervals(history: &[Event], key: &Tuple, axis: Axis, fixed: i64) -> Vec<Interval> {
    let events: Vec<&Event> = history.iter().filter(|e| e.key() == key).collect();
    let mut breaks: Vec<i64> = match axis {
        Axis::Valid => events.iter().map(|e| e.valid()).collect(),
        Axis::Sys => events
            .iter()
            .filter(|e| e.valid() <= fixed)
            .map(|e| e.sys())
            .collect(),
    };
    breaks.sort_unstable();
    breaks.dedup();
    let coordinate = |pt: i64| -> AsOf {
        match axis {
            Axis::Valid => AsOf {
                valid: pt,
                sys: fixed,
            },
            Axis::Sys => AsOf {
                valid: fixed,
                sys: pt,
            },
        }
    };

    let mut out = Vec::new();
    let mut i = 0;
    while i < breaks.len() {
        let start = breaks[i];
        let Some(tuple) = resolve_events(&events, coordinate(start)) else {
            i += 1;
            continue;
        };
        let mut j = i;
        while j + 1 < breaks.len()
            && resolve_events(&events, coordinate(breaks[j + 1])).as_ref() == Some(&tuple)
        {
            j += 1;
        }
        let end = if j + 1 < breaks.len() {
            breaks[j + 1]
        } else {
            OPEN_END
        };
        out.push(Interval { start, end, tuple });
        i = j + 1;
    }
    out
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SignedFact {
    Plus(Tuple),
    Minus(Tuple),
}

pub fn diff(history: &[Event], from: AsOf, to: AsOf) -> BTreeSet<SignedFact> {
    let a = resolve_relation(history, from);
    let b = resolve_relation(history, to);
    let mut out = BTreeSet::new();
    for t in a.difference(&b) {
        out.insert(SignedFact::Minus(t.clone()));
    }
    for t in b.difference(&a) {
        out.insert(SignedFact::Plus(t.clone()));
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposeNetOutOfRange {
    pub net: i32,
}

impl fmt::Display for ComposeNetOutOfRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "compose net tally {} is outside {{-1, 0, +1}}", self.net)
    }
}

impl std::error::Error for ComposeNetOutOfRange {}

pub fn compose(
    first: &BTreeSet<SignedFact>,
    second: &BTreeSet<SignedFact>,
) -> Result<BTreeSet<SignedFact>, ComposeNetOutOfRange> {
    let mut tally: BTreeMap<&Tuple, i32> = BTreeMap::new();
    for patch in [first, second] {
        for fact in patch {
            let (t, delta) = match fact {
                SignedFact::Plus(t) => (t, 1),
                SignedFact::Minus(t) => (t, -1),
            };
            *tally.entry(t).or_insert(0) += delta;
        }
    }
    let mut out = BTreeSet::new();
    for (t, net) in tally {
        match net {
            0 => {}
            1 => {
                out.insert(SignedFact::Plus(t.clone()));
            }
            -1 => {
                out.insert(SignedFact::Minus(t.clone()));
            }
            n => return Err(ComposeNetOutOfRange { net: n }),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kyzo_model::value::DataValue;

    fn k(i: i64) -> Tuple {
        Tuple::from_vec(vec![DataValue::from(i)])
    }
    fn pay(i: i64) -> Tuple {
        Tuple::from_vec(vec![DataValue::from(i)])
    }
    fn kv(i: i64, p: i64) -> Tuple {
        Tuple::from_vec(vec![DataValue::from(i), DataValue::from(p)])
    }
    fn ev_assert(key: Tuple, payload: Tuple, valid: i64, sys: i64) -> Event {
        Event::assert(key, payload, valid, sys).expect("valid instant")
    }
    fn ev_retract(key: Tuple, valid: i64, sys: i64) -> Event {
        Event::retract(key, valid, sys).expect("valid instant")
    }
    fn ev_erase(key: Tuple, valid: i64, sys: i64) -> Event {
        Event::erase(key, valid, sys).expect("valid instant")
    }

    #[test]
    fn retract_clips_start_to_retract_exclusive() {
        let key = k(1);
        let history = vec![
            ev_assert(key.clone(), pay(10), 0, 0),
            ev_retract(key.clone(), 5, 1),
        ];
        let ivs = derive_intervals(&history, &key, Axis::Valid, AsOf::current().sys);
        assert_eq!(
            ivs,
            vec![Interval {
                start: 0,
                end: 5,
                tuple: kv(1, 10),
            }]
        );
    }

    #[test]
    fn dangling_retract_blocks_erase_fall_through() {
        let key = k(1);
        let history = vec![
            ev_assert(key.clone(), pay(10), 0, 0),
            ev_erase(key.clone(), 0, 1),
            ev_retract(key.clone(), 5, 2),
        ];
        assert_eq!(resolve(&history, &key, AsOf::current_at(4)), None);
        assert_eq!(resolve(&history, &key, AsOf::current_at(5)), None);
    }

    #[test]
    fn double_assert_same_payload_is_idempotent_one_interval() {
        let key = k(1);
        let history = vec![
            ev_assert(key.clone(), pay(10), 0, 0),
            ev_assert(key.clone(), pay(10), 5, 1),
        ];
        let ivs = derive_intervals(&history, &key, Axis::Valid, AsOf::current().sys);
        assert_eq!(
            ivs,
            vec![Interval {
                start: 0,
                end: OPEN_END,
                tuple: kv(1, 10),
            }]
        );
    }

    #[test]
    fn double_assert_different_payload_splits_at_the_second_assert() {
        let key = k(1);
        let history = vec![
            ev_assert(key.clone(), pay(10), 0, 0),
            ev_assert(key.clone(), pay(20), 5, 1),
        ];
        let ivs = derive_intervals(&history, &key, Axis::Valid, AsOf::current().sys);
        assert_eq!(
            ivs,
            vec![
                Interval {
                    start: 0,
                    end: 5,
                    tuple: kv(1, 10),
                },
                Interval {
                    start: 5,
                    end: OPEN_END,
                    tuple: kv(1, 20),
                },
            ]
        );
    }

    #[test]
    fn assert_after_retract_opens_a_new_interval() {
        let key = k(1);
        let history = vec![
            ev_assert(key.clone(), pay(10), 0, 0),
            ev_retract(key.clone(), 5, 1),
            ev_assert(key.clone(), pay(30), 10, 2),
        ];
        let ivs = derive_intervals(&history, &key, Axis::Valid, AsOf::current().sys);
        assert_eq!(
            ivs,
            vec![
                Interval {
                    start: 0,
                    end: 5,
                    tuple: kv(1, 10),
                },
                Interval {
                    start: 10,
                    end: OPEN_END,
                    tuple: kv(1, 30),
                },
            ]
        );
    }

    #[test]
    fn assert_then_retract_same_instant_newer_sys_holds_nowhere() {
        let key = k(1);
        let history = vec![
            ev_assert(key.clone(), pay(10), 5, 0),
            ev_retract(key.clone(), 5, 1),
        ];
        let ivs = derive_intervals(&history, &key, Axis::Valid, AsOf::current().sys);
        assert!(ivs.is_empty());
    }

    #[test]
    fn erase_is_transparent_to_intervals() {
        let key = k(1);
        let history = vec![
            ev_assert(key.clone(), pay(10), 0, 0),
            ev_erase(key.clone(), 0, 1),
            ev_assert(key.clone(), pay(10), 5, 2),
        ];
        // After erase of the first assert, fall-through finds nothing until
        // the second assert — but erase is transparent at its own instant
        // only when an older assert still governs; here the erased version
        // was the only one at valid=0, so resolve at 0..5 is None.
        assert_eq!(resolve(&history, &key, AsOf::current_at(0)), None);
        assert_eq!(
            resolve(&history, &key, AsOf::current_at(5)),
            Some(kv(1, 10))
        );
    }

    #[test]
    fn instants_are_one_tick_no_zero_width_intervals() {
        let key = k(1);
        let history = vec![
            ev_assert(key.clone(), pay(10), 0, 0),
            ev_assert(key.clone(), pay(20), 1, 1),
        ];
        let ivs = derive_intervals(&history, &key, Axis::Valid, AsOf::current().sys);
        for iv in &ivs {
            assert!(iv.end > iv.start, "no zero-width interval: {iv:?}");
        }
    }

    #[test]
    fn system_axis_interval_of_a_version_is_stamp_to_next_version_stamp() {
        let key = k(1);
        let history = vec![
            ev_assert(key.clone(), pay(10), 0, 0),
            ev_assert(key.clone(), pay(20), 0, 5),
        ];
        let ivs = derive_intervals(&history, &key, Axis::Sys, 0);
        assert_eq!(
            ivs,
            vec![
                Interval {
                    start: 0,
                    end: 5,
                    tuple: kv(1, 10),
                },
                Interval {
                    start: 5,
                    end: OPEN_END,
                    tuple: kv(1, 20),
                },
            ]
        );
    }

    #[test]
    fn terminal_tick_is_reserved_and_refused_at_construction() {
        let err = Event::assert(k(1), pay(10), i64::MAX, 0)
            .expect_err("the terminal tick must be refused");
        assert!(
            err.to_string().contains("reserved"),
            "expected a reservation error, got: {err}"
        );
    }

    #[test]
    fn terminal_tick_never_produces_a_zero_width_interval() {
        let key = k(1);
        let history = vec![ev_assert(key.clone(), pay(10), 0, 0)];
        let err = Event::assert(key.clone(), pay(20), i64::MAX, 1)
            .expect_err("the terminal tick must be refused, not silently accepted");
        assert!(
            err.to_string().contains("reserved"),
            "expected a reservation error, got: {err}"
        );
        let ivs = derive_intervals(&history, &key, Axis::Valid, AsOf::current().sys);
        for iv in &ivs {
            assert!(iv.end > iv.start, "no zero-width interval: {iv:?}");
        }
    }

    #[test]
    fn diff_on_resolved_snapshots_never_intervals() {
        let key = k(1);
        let history = vec![
            ev_assert(key.clone(), pay(10), 0, 0),
            ev_retract(key.clone(), 5, 1),
            ev_assert(key.clone(), pay(30), 10, 2),
        ];
        let d = diff(
            &history,
            AsOf {
                valid: 3,
                sys: i64::MAX,
            },
            AsOf {
                valid: 12,
                sys: i64::MAX,
            },
        );
        assert_eq!(
            d,
            [SignedFact::Minus(kv(1, 10)), SignedFact::Plus(kv(1, 30)),]
                .into_iter()
                .collect()
        );
    }

    #[test]
    fn compose_cancels_round_trip_payload_change() {
        let a: BTreeSet<_> = [SignedFact::Minus(kv(1, 10)), SignedFact::Plus(kv(1, 20))]
            .into_iter()
            .collect();
        let b: BTreeSet<_> = [SignedFact::Minus(kv(1, 20)), SignedFact::Plus(kv(1, 10))]
            .into_iter()
            .collect();
        let c = compose(&a, &b).unwrap();
        assert!(c.is_empty());
    }

    #[test]
    fn claim_polarity_maps_from_event_kinds() {
        let events = [
            ev_assert(k(1), pay(1), 0, 0),
            ev_retract(k(1), 1, 1),
            ev_erase(k(1), 2, 2),
        ];
        let kinds: Vec<_> = events
            .iter()
            .map(|e| match e {
                Event::Assert { .. } => ClaimPolarity::Assert,
                Event::Retract { .. } => ClaimPolarity::Retract,
                Event::Erase { .. } => ClaimPolarity::Erase,
            })
            .collect();
        assert_eq!(
            kinds,
            [
                ClaimPolarity::Assert,
                ClaimPolarity::Retract,
                ClaimPolarity::Erase
            ]
        );
    }
}
