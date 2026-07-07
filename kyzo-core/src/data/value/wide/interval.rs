/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! `Interval`: a pure value over valid-time microseconds — no ambient
//! clock, no read-time normalization. The domain is the **discrete i64
//! grid**, and the identity law follows from that:
//!
//! - **Closed normal form.** On a discrete domain, `[1,5)` and `[1,4]`
//!   denote the same set and must be ONE identity, so every bound
//!   normalizes to inclusive form at construction: an open start `(t`
//!   becomes `[t+1`, an open end `t)` becomes `t-1]`, saturating into
//!   emptiness at the domain edges. `(t, t+1)` is empty on the grid.
//! - **Empty is one value.** Any pair denoting the empty set collapses
//!   to [`Interval::EMPTY`] at construction; a denotes-empty range
//!   cannot be written down.
//! - Unbounded ends are explicit canonical forms (`-inf` start, `+inf`
//!   end), distinct from any finite bound.
//! - Storage order (via canonical bytes) is deterministic and total but
//!   is NOT a semantic interval order — Allen's relations are the
//!   expression-level authority, separate and refusable.
//!
//! Canonical payload (format v1): `0x01` for the one empty value, or
//! `0x02` then lower end then upper end, where each end is `0x01`
//! (unbounded) or `0x02` followed by the 8-byte ascending timestamp key
//! (sign-flipped big-endian i64). Decode refuses unknown markers,
//! truncation, and any range denoting empty. Note: `Hi::At(i64::MAX)`
//! and `Hi::PosUnbounded` are DISTINCT values here — the finite grid
//! maximum is an instant, unbounded is the absence of a bound; the
//! temporal tuple layer's open-end sentinels are its own vocabulary,
//! never conflated with this kind's.

/// A user-facing bound for constructing intervals; normalized away at
/// construction (the canonical form is closed).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Bound {
    Unbounded,
    Closed(i64),
    Open(i64),
}

/// A canonical lower end: `-inf` or an inclusive instant.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Lo {
    NegUnbounded,
    At(i64),
}

/// A canonical upper end: an inclusive instant or `+inf`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Hi {
    At(i64),
    PosUnbounded,
}

/// A canonical interval: the one empty value, or a closed-normal-form
/// range with `lo <= hi`. Constructible only through [`Interval::new`] /
/// [`Interval::EMPTY`]; unlawful forms cannot be written down.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Interval(Form);

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Form {
    Empty,
    Range { lo: Lo, hi: Hi },
}

impl Interval {
    pub const EMPTY: Interval = Interval(Form::Empty);

    /// Canonicalizing constructor: bounds normalize to closed form on
    /// the discrete grid; empty denotations collapse to EMPTY.
    pub fn new(start: Bound, end: Bound) -> Interval {
        let lo = match start {
            Bound::Unbounded => Lo::NegUnbounded,
            Bound::Closed(t) => Lo::At(t),
            Bound::Open(t) => match t.checked_add(1) {
                Some(t1) => Lo::At(t1),
                None => return Interval::EMPTY, // (i64::MAX, .. is empty
            },
        };
        let hi = match end {
            Bound::Unbounded => Hi::PosUnbounded,
            Bound::Closed(t) => Hi::At(t),
            Bound::Open(t) => match t.checked_sub(1) {
                Some(t1) => Hi::At(t1),
                None => return Interval::EMPTY, // .., i64::MIN) is empty
            },
        };
        if let (Lo::At(l), Hi::At(h)) = (lo, hi)
            && l > h
        {
            return Interval::EMPTY;
        }
        Interval(Form::Range { lo, hi })
    }

    /// Construct directly from canonical ends (already closed form).
    pub fn range(lo: Lo, hi: Hi) -> Interval {
        if let (Lo::At(l), Hi::At(h)) = (lo, hi)
            && l > h
        {
            return Interval::EMPTY;
        }
        Interval(Form::Range { lo, hi })
    }

    pub fn is_empty(self) -> bool {
        matches!(self.0, Form::Empty)
    }

    /// The canonical ends of a non-empty interval.
    /// The closed start instant, if one exists (`None` for the empty
    /// interval and for a negatively-unbounded start).
    pub fn start(self) -> Option<i64> {
        match self.ends() {
            Some((Lo::At(t), _)) => Some(t),
            _ => None,
        }
    }

    /// The closed end instant, if one exists (`None` for the empty
    /// interval and for a positively-unbounded end).
    pub fn end(self) -> Option<i64> {
        match self.ends() {
            Some((_, Hi::At(t))) => Some(t),
            _ => None,
        }
    }

    /// The ends widened to i128 sentinels for relation arithmetic
    /// (unbounded ends become the i128 extremes, so `+1` never
    /// overflows). `None` for the empty interval.
    fn wide_ends(self) -> Option<(i128, i128)> {
        self.ends().map(|(lo, hi)| {
            let l = match lo {
                Lo::NegUnbounded => i128::MIN,
                Lo::At(t) => t as i128,
            };
            let h = match hi {
                Hi::PosUnbounded => i128::MAX,
                Hi::At(t) => t as i128,
            };
            (l, h)
        })
    }

    /// Allen's relations on the discrete closed normal form. The empty
    /// interval satisfies NO relation (both operands must be nonempty),
    /// and on the discrete grid adjacency is `a.hi + 1 == b.lo`: `meets`
    /// is exactly adjacency, `before` requires a gap — together with
    /// `overlaps`/`starts`/`during`/`finishes`/equality (the generic
    /// `eq`) and the argument-swapped inverses, every configuration of
    /// two nonempty intervals satisfies exactly one relation.
    pub fn before(self, other: Interval) -> bool {
        match (self.wide_ends(), other.wide_ends()) {
            // checked successor: an end at the +inf sentinel has none,
            // and is before nothing (the unchecked `+ 1` wrapped).
            (Some((_, ah)), Some((bl, _))) => ah.checked_add(1).is_some_and(|s| s < bl),
            _ => false,
        }
    }

    pub fn meets(self, other: Interval) -> bool {
        match (self.wide_ends(), other.wide_ends()) {
            (Some((_, ah)), Some((bl, _))) => ah.checked_add(1) == Some(bl),
            _ => false,
        }
    }

    pub fn overlaps(self, other: Interval) -> bool {
        match (self.wide_ends(), other.wide_ends()) {
            (Some((al, ah)), Some((bl, bh))) => al < bl && bl <= ah && ah < bh,
            _ => false,
        }
    }

    pub fn starts(self, other: Interval) -> bool {
        match (self.wide_ends(), other.wide_ends()) {
            (Some((al, ah)), Some((bl, bh))) => al == bl && ah < bh,
            _ => false,
        }
    }

    pub fn during(self, other: Interval) -> bool {
        match (self.wide_ends(), other.wide_ends()) {
            (Some((al, ah)), Some((bl, bh))) => bl < al && ah < bh,
            _ => false,
        }
    }

    pub fn finishes(self, other: Interval) -> bool {
        match (self.wide_ends(), other.wide_ends()) {
            (Some((al, ah)), Some((bl, bh))) => ah == bh && bl < al,
            _ => false,
        }
    }

    /// Nonempty intersection — the workhorse predicate.
    pub fn intersects(self, other: Interval) -> bool {
        match (self.wide_ends(), other.wide_ends()) {
            (Some((al, ah)), Some((bl, bh))) => al.max(bl) <= ah.min(bh),
            _ => false,
        }
    }

    pub fn ends(self) -> Option<(Lo, Hi)> {
        match self.0 {
            Form::Empty => None,
            Form::Range { lo, hi } => Some((lo, hi)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Allen partition law: over all nonempty bounded pairs in a small
    /// grid, exactly ONE of the 13 relations (6 primitives + 6 inverses
    /// + equality) holds.
    #[test]
    fn allen_relations_partition_the_configurations() {
        let mut ivs = Vec::new();
        for lo in -3..=3i64 {
            for hi in lo..=3i64 {
                ivs.push(Interval::new(Bound::Closed(lo), Bound::Closed(hi)));
            }
        }
        ivs.push(Interval::range(Lo::NegUnbounded, Hi::At(0)));
        ivs.push(Interval::range(Lo::At(0), Hi::PosUnbounded));
        ivs.push(Interval::range(Lo::NegUnbounded, Hi::PosUnbounded));
        for &a in &ivs {
            for &b in &ivs {
                let rels = [
                    a.before(b),
                    b.before(a),
                    a.meets(b),
                    b.meets(a),
                    a.overlaps(b),
                    b.overlaps(a),
                    a.starts(b),
                    b.starts(a),
                    a.during(b),
                    b.during(a),
                    a.finishes(b),
                    b.finishes(a),
                    a == b,
                ];
                assert_eq!(
                    rels.iter().filter(|&&r| r).count(),
                    1,
                    "partition violated for {a:?} vs {b:?}: {rels:?}"
                );
                // intersects consistency: true iff not (before/meets either way).
                assert_eq!(
                    a.intersects(b),
                    !(a.before(b) || b.before(a) || a.meets(b) || b.meets(a)),
                    "intersects law violated for {a:?} vs {b:?}"
                );
            }
        }
        // The empty interval satisfies no relation, not even with itself.
        let e = Interval::EMPTY;
        let some = ivs[0];
        assert!(!e.before(some) && !some.before(e) && !e.intersects(e) && !e.starts(e));
    }

    #[test]
    fn closed_normal_form_is_one_identity() {
        // [1,5) == [1,4] on the discrete grid.
        assert_eq!(
            Interval::new(Bound::Closed(1), Bound::Open(5)),
            Interval::new(Bound::Closed(1), Bound::Closed(4))
        );
        // (0, .. == [1, ..
        assert_eq!(
            Interval::new(Bound::Open(0), Bound::Unbounded),
            Interval::new(Bound::Closed(1), Bound::Unbounded)
        );
    }

    #[test]
    fn empty_denotations_are_one_value() {
        assert_eq!(
            Interval::new(Bound::Closed(5), Bound::Open(5)),
            Interval::EMPTY
        );
        assert_eq!(
            Interval::new(Bound::Open(5), Bound::Open(6)),
            Interval::EMPTY
        );
        assert_eq!(
            Interval::new(Bound::Closed(9), Bound::Closed(1)),
            Interval::EMPTY
        );
        assert_eq!(
            Interval::new(Bound::Open(i64::MAX), Bound::Unbounded),
            Interval::EMPTY
        );
        assert_eq!(
            Interval::new(Bound::Unbounded, Bound::Open(i64::MIN)),
            Interval::EMPTY
        );
        // Singleton and full-line are proper.
        assert!(!Interval::new(Bound::Closed(5), Bound::Closed(5)).is_empty());
        assert!(!Interval::new(Bound::Unbounded, Bound::Unbounded).is_empty());
    }
}
