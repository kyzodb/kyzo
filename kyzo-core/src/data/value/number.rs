/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! `Num`: the unified sortable int/float space (the `Num` tag's inline realization).
//!
//! ## The numeric identity law (format v1, ruled before any byte)
//!
//! The domain is `i64 ∪ f64` under **one** top-level tag, totally ordered
//! by **exact real value**, with a representation tie-break:
//!
//! - Order: real-number order, computed exactly (never through a lossy
//!   cast) — every integer interleaves correctly with every float,
//!   including beyond 2^53 where floats cannot represent them.
//! - Identity is representation-faithful: `Int(1)` and `Float(1.0)` are
//!   *adjacent, not equal* — equal reals order `Int < Float`, uniformly,
//!   both signs. (Unifying them would make round-trip lossy: aggregates
//!   producing `2.0` must not come back as `2`.)
//! - `-0.0` normalizes to `+0.0` at construction: two floats that compare
//!   equal must be one identity, or dedup splits equal things forever.
//! - All NaN bit patterns collapse to one canonical NaN, which is a
//!   `Float`, equal to itself, and **greatest**: above `+∞`.
//! - `±∞` order as the finite extremes' neighbors.
//!
//! Consequence: equivalence = dedup identity = code identity, and numeric
//! *order* crosses representations while numeric *identity* does not.
//!
//! Two durable rulings riding on this law:
//! - `Int(1) != Float(1.0)` is **query-layer semantics forever**, not an
//!   encoding detail — the seam's oracle differentials gate that the
//!   expression layer agrees.
//! - The v1 numeric domain is closed: exactly `i64 ∪ f64`. Decimal or
//!   bigint, if they ever earn existence, are *new kinds* in reserved tag
//!   space with their own identity laws — never extensions of this key,
//!   whose byte layout is permanent.
//!
//! ## The order-preserving key (format v1)
//!
//! `[class][exponent 2B][fraction 9B][repr]`, where class is `NEG=0x01 <
//! ZERO=0x02 < POS=0x03 < NAN=0x04` (zero: `[0x02][repr]`; NaN:
//! `[0x04][0x01]`). A nonzero finite magnitude is written normalized as
//! `f × 2^E` with `f ∈ [0.5, 1)`: the exponent as `E + 1080` big-endian
//! (covering subnormal 2^-1074 through 2^1024 and every i64), the
//! fraction as 72 big-endian bits with the leading 1 at the top (64 bits
//! hold any i64 exactly, 53 any f64 significand). `±∞` is the sentinel
//! `exp=0xFFFF, fraction=0xFF…`. Negative keys complement exponent and
//! fraction bytes so order reverses; the trailing repr byte (`0=Int`,
//! `1=Float`) is never complemented — the tie-break is type order, not
//! value order. Lexicographic byte order of keys equals the semantic
//! order above; the law is property-tested against an independent exact
//! comparator, both directions.

use std::cmp::Ordering;

/// Class bytes of the numeric key.
const CLASS_NEG: u8 = 0x01;
const CLASS_ZERO: u8 = 0x02;
const CLASS_POS: u8 = 0x03;
const CLASS_NAN: u8 = 0x04;

/// Repr tie-break bytes.
const REPR_INT: u8 = 0x00;
const REPR_FLOAT: u8 = 0x01;

/// Exponent offset: E ∈ [-1073, 1024] for finite values → stored E+1080.
const EXP_OFFSET: i32 = 1080;
/// Infinity sentinel exponent bytes (beyond every finite offset).
const EXP_INF: u16 = 0xFFFF;

/// One canonical NaN bit pattern (positive quiet NaN).
const CANON_NAN_BITS: u64 = 0x7FF8_0000_0000_0000;

/// A number of the unified domain: exactly an `i64` or an `f64`, held in
/// normalized identity form (no `-0.0`, one NaN). Construct through
/// [`Num::int`] / [`Num::float`]; `Ord` is the semantic law (exact real
/// order, `Int < Float` on equal reals, NaN greatest) — lawful as a trait
/// here because `Num` is fully inline: no deref, no context.
#[derive(Clone, Copy, Debug)]
pub struct Num(Repr);

#[derive(Clone, Copy, Debug)]
enum Repr {
    Int(i64),
    Float(f64),
}

impl Num {
    pub fn int(v: i64) -> Num {
        Num(Repr::Int(v))
    }

    /// Construct a float, applying the identity law: `-0.0 → +0.0`, any
    /// NaN → the canonical NaN.
    pub fn float(v: f64) -> Num {
        if v.is_nan() {
            return Num(Repr::Float(f64::from_bits(CANON_NAN_BITS)));
        }
        if v == 0.0 {
            return Num(Repr::Float(0.0));
        }
        Num(Repr::Float(v))
    }

    /// NUMERIC comparison: exact real-value order with ties EQUAL —
    /// `1 == 1.0` here, unlike the identity/storage order where they are
    /// adjacent distinct values. This is the expression layer's
    /// comparison authority for numbers; `Ord` remains the storage
    /// mirror. Two authorities, both named, never confused.
    pub fn cmp_numeric(self, other: Num) -> Ordering {
        // The total order is (real value, repr tie-break); stripping the
        // tie-break yields the numeric order exactly.
        match self.cmp(&other) {
            Ordering::Equal => Ordering::Equal,
            o => {
                if self.repr_byte() != other.repr_byte() {
                    // Could be a pure tie-break difference: re-check by
                    // comparing with reprs swapped-normalized.
                    let same = matches!(
                        (self.as_int(), other.as_float()),
                        (Some(i), Some(f)) if int_float_eq(i, f)
                    ) || matches!(
                        (self.as_float(), other.as_int()),
                        (Some(f), Some(i)) if int_float_eq(i, f)
                    );
                    if same { Ordering::Equal } else { o }
                } else {
                    o
                }
            }
        }
    }

    /// Numeric equality (see [`Num::cmp_numeric`]).
    pub fn eq_numeric(self, other: Num) -> bool {
        self.cmp_numeric(other) == Ordering::Equal
    }

    /// Numeric maximum (ties keep `self` — deterministic and
    /// accumulation-friendly). Expression authority, like
    /// [`Num::cmp_numeric`].
    pub fn max_numeric(self, other: Num) -> Num {
        if self.cmp_numeric(other) == Ordering::Less {
            other
        } else {
            self
        }
    }

    /// Numeric minimum (ties keep `self`).
    pub fn min_numeric(self, other: Num) -> Num {
        if self.cmp_numeric(other) == Ordering::Greater {
            other
        } else {
            self
        }
    }

    /// The numeric value as f64: ints promote by cast. A NUMERIC read
    /// for math kernels, never an identity claim (beyond 2^53 the cast
    /// rounds; comparison sites must use [`Num::cmp_numeric`] instead).
    pub fn to_f64(self) -> f64 {
        match self.repr() {
            NumRepr::Int(i) => i as f64,
            NumRepr::Float(f) => f,
        }
    }

    /// The read-only representation view: pattern-matching ergonomics
    /// WITHOUT construction authority — `NumRepr` cannot be turned back
    /// into a `Num` except through the normalizing constructors, so the
    /// identity law (`-0.0` collapsed, one NaN) cannot be bypassed by a
    /// public variant.
    pub fn repr(self) -> NumRepr {
        match self.0 {
            Repr::Int(v) => NumRepr::Int(v),
            Repr::Float(v) => NumRepr::Float(v),
        }
    }

    pub fn as_int(self) -> Option<i64> {
        match self.0 {
            Repr::Int(v) => Some(v),
            Repr::Float(_) => None,
        }
    }

    pub fn as_float(self) -> Option<f64> {
        match self.0 {
            Repr::Float(v) => Some(v),
            Repr::Int(_) => None,
        }
    }

    fn repr_byte(self) -> u8 {
        match self.0 {
            Repr::Int(_) => REPR_INT,
            Repr::Float(_) => REPR_FLOAT,
        }
    }

    /// Append the order-preserving key.
    pub fn encode_key(self, out: &mut Vec<u8>) {
        match self.0 {
            Repr::Int(0) => {
                out.push(CLASS_ZERO);
                out.push(REPR_INT);
            }
            Repr::Float(v) if v.to_bits() == 0 => {
                out.push(CLASS_ZERO);
                out.push(REPR_FLOAT);
            }
            Repr::Float(v) if v.is_nan() => {
                out.push(CLASS_NAN);
                out.push(REPR_FLOAT);
            }
            _ => {
                let (neg, mag) = self.sign_magnitude();
                out.push(if neg { CLASS_NEG } else { CLASS_POS });
                let start = out.len();
                match mag {
                    Magnitude::Inf => {
                        out.extend_from_slice(&EXP_INF.to_be_bytes());
                        out.extend_from_slice(&[0xFF; 9]);
                    }
                    Magnitude::Finite { e, frac72 } => {
                        let off = (e + EXP_OFFSET) as u16;
                        debug_assert!(off < EXP_INF);
                        out.extend_from_slice(&off.to_be_bytes());
                        let fb = frac72.to_be_bytes(); // 16 bytes; the low 9 hold the 72-bit field
                        out.extend_from_slice(&fb[7..16]);
                    }
                }
                if neg {
                    for b in &mut out[start..] {
                        *b = !*b;
                    }
                }
                out.push(self.repr_byte());
            }
        }
    }

    /// Decode a key from the front of `bytes`, returning the value and
    /// the number of bytes consumed. Total: malformed input is a typed
    /// error, never a panic.
    pub fn decode_key(bytes: &[u8]) -> Result<(Num, usize), NumDecodeError> {
        let class = *bytes.first().ok_or(NumDecodeError::Truncated)?;
        match class {
            CLASS_ZERO => match bytes.get(1) {
                Some(&REPR_INT) => Ok((Num::int(0), 2)),
                Some(&REPR_FLOAT) => Ok((Num::float(0.0), 2)),
                Some(_) => Err(NumDecodeError::BadRepr),
                None => Err(NumDecodeError::Truncated),
            },
            CLASS_NAN => match bytes.get(1) {
                Some(&REPR_FLOAT) => Ok((Num::float(f64::NAN), 2)),
                Some(_) => Err(NumDecodeError::BadRepr),
                None => Err(NumDecodeError::Truncated),
            },
            CLASS_NEG | CLASS_POS => {
                if bytes.len() < 13 {
                    return Err(NumDecodeError::Truncated);
                }
                let neg = class == CLASS_NEG;
                let mut body = [0u8; 11];
                body.copy_from_slice(&bytes[1..12]);
                if neg {
                    for b in &mut body {
                        *b = !*b;
                    }
                }
                let repr = bytes[12];
                let exp = u16::from_be_bytes([body[0], body[1]]);
                let mut frac_bytes = [0u8; 16];
                frac_bytes[7..16].copy_from_slice(&body[2..11]);
                let frac72 = u128::from_be_bytes(frac_bytes);
                let value = if exp == EXP_INF {
                    if frac72 != (1u128 << 72) - 1 || repr != REPR_FLOAT {
                        return Err(NumDecodeError::BadInfinity);
                    }
                    Num::float(if neg {
                        f64::NEG_INFINITY
                    } else {
                        f64::INFINITY
                    })
                } else {
                    let e = exp as i32 - EXP_OFFSET;
                    if frac72 >> 71 != 1 {
                        return Err(NumDecodeError::Denormalized);
                    }
                    match repr {
                        REPR_INT => Self::rebuild_int(neg, e, frac72)?,
                        REPR_FLOAT => Self::rebuild_float(neg, e, frac72)?,
                        _ => return Err(NumDecodeError::BadRepr),
                    }
                };
                Ok((value, 13))
            }
            _ => Err(NumDecodeError::BadClass),
        }
    }

    fn rebuild_int(neg: bool, e: i32, frac72: u128) -> Result<Num, NumDecodeError> {
        if !(1..=64).contains(&e) {
            return Err(NumDecodeError::IntRange);
        }
        let shift = 72 - e as u32;
        if frac72.trailing_zeros() < shift {
            return Err(NumDecodeError::IntRange);
        }
        let m = (frac72 >> shift) as u64;
        if neg {
            if m > 1u64 << 63 {
                return Err(NumDecodeError::IntRange);
            }
            Ok(Num::int((m as i128).wrapping_neg() as i64))
        } else {
            if m > i64::MAX as u64 {
                return Err(NumDecodeError::IntRange);
            }
            Ok(Num::int(m as i64))
        }
    }

    fn rebuild_float(neg: bool, e: i32, frac72: u128) -> Result<Num, NumDecodeError> {
        // Normal floats: value = 0.1xxx… × 2^E with 53 significant bits.
        if e >= -1021 {
            if e > 1024 {
                return Err(NumDecodeError::FloatRange);
            }
            if frac72.trailing_zeros() < 19 {
                return Err(NumDecodeError::FloatRange);
            }
            let sig53 = (frac72 >> 19) as u64;
            let expf = (e - 1 + 1023) as u64;
            let bits = (expf << 52) | (sig53 & ((1u64 << 52) - 1));
            let v = f64::from_bits(bits);
            Ok(Num::float(if neg { -v } else { v }))
        } else {
            // Subnormal: value = frac52 × 2^-1074, E = bitlen(frac52) - 1074.
            let bl = e + 1074;
            if !(1..=52).contains(&bl) {
                return Err(NumDecodeError::FloatRange);
            }
            let shift = 72 - bl as u32;
            if frac72.trailing_zeros() < shift {
                return Err(NumDecodeError::FloatRange);
            }
            let frac52 = (frac72 >> shift) as u64;
            let v = f64::from_bits(frac52);
            Ok(Num::float(if neg { -v } else { v }))
        }
    }

    /// Sign and normalized magnitude of a nonzero, non-NaN number.
    fn sign_magnitude(self) -> (bool, Magnitude) {
        match self.0 {
            Repr::Int(v) => {
                debug_assert!(v != 0);
                let neg = v < 0;
                let m = v.unsigned_abs();
                let bl = 64 - m.leading_zeros();
                let e = bl as i32;
                let frac72 = (m as u128) << (72 - bl);
                (neg, Magnitude::Finite { e, frac72 })
            }
            Repr::Float(v) => {
                debug_assert!(v != 0.0 && !v.is_nan());
                let neg = v < 0.0;
                if v.is_infinite() {
                    return (neg, Magnitude::Inf);
                }
                let bits = v.abs().to_bits();
                let expf = (bits >> 52) as i32;
                let frac52 = bits & ((1u64 << 52) - 1);
                if expf > 0 {
                    // Normal: 1.frac × 2^(expf-1023) = 0.1frac × 2^E.
                    let sig53 = (1u64 << 52) | frac52;
                    let e = expf - 1023 + 1;
                    let frac72 = (sig53 as u128) << 19;
                    (neg, Magnitude::Finite { e, frac72 })
                } else {
                    // Subnormal: frac52 × 2^-1074.
                    let bl = 64 - frac52.leading_zeros();
                    let e = bl as i32 - 1074;
                    let frac72 = (frac52 as u128) << (72 - bl);
                    (neg, Magnitude::Finite { e, frac72 })
                }
            }
        }
    }

    fn class(self) -> u8 {
        match self.0 {
            Repr::Int(0) => CLASS_ZERO,
            Repr::Float(v) if v.is_nan() => CLASS_NAN,
            Repr::Float(v) if v.to_bits() == 0 => CLASS_ZERO,
            Repr::Int(v) => {
                if v < 0 {
                    CLASS_NEG
                } else {
                    CLASS_POS
                }
            }
            Repr::Float(v) => {
                if v < 0.0 {
                    CLASS_NEG
                } else {
                    CLASS_POS
                }
            }
        }
    }
}

enum Magnitude {
    Finite { e: i32, frac72: u128 },
    Inf,
}

impl PartialEq for Num {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Num {}

impl PartialOrd for Num {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Num {
    /// The semantic law: exact real order, `Int < Float` on equal reals,
    /// NaN greatest and equal to itself.
    fn cmp(&self, other: &Self) -> Ordering {
        let (ca, cb) = (self.class(), other.class());
        if ca != cb {
            return ca.cmp(&cb);
        }
        match ca {
            CLASS_ZERO | CLASS_NAN => self.repr_byte().cmp(&other.repr_byte()),
            _ => {
                let (neg, ma) = self.sign_magnitude();
                let (_, mb) = other.sign_magnitude();
                let mag = match (ma, mb) {
                    (Magnitude::Inf, Magnitude::Inf) => Ordering::Equal,
                    (Magnitude::Inf, Magnitude::Finite { .. }) => Ordering::Greater,
                    (Magnitude::Finite { .. }, Magnitude::Inf) => Ordering::Less,
                    (
                        Magnitude::Finite { e: ea, frac72: fa },
                        Magnitude::Finite { e: eb, frac72: fb },
                    ) => ea.cmp(&eb).then(fa.cmp(&fb)),
                };
                let real = if neg { mag.reverse() } else { mag };
                real.then(self.repr_byte().cmp(&other.repr_byte()))
            }
        }
    }
}

impl std::hash::Hash for Num {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        match self.0 {
            Repr::Int(v) => {
                state.write_u8(REPR_INT);
                state.write_i64(v);
            }
            Repr::Float(v) => {
                state.write_u8(REPR_FLOAT);
                state.write_u64(v.to_bits());
            }
        }
    }
}

/// Exact int/float real-value equality (no lossy casts): true iff the
/// float is integral, in range, and equal.
fn int_float_eq(i: i64, f: f64) -> bool {
    if !f.is_finite() || f.fract() != 0.0 {
        return false;
    }
    if !(-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&f) {
        return false;
    }
    f as i64 == i
}

/// The read-only view of a `Num`'s representation (see [`Num::repr`]):
/// match on it freely; mint through the normalizing constructors only.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum NumRepr {
    Int(i64),
    Float(f64),
}

/// Typed decode failures: total input handling, never a panic.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NumDecodeError {
    Truncated,
    BadClass,
    BadRepr,
    BadInfinity,
    Denormalized,
    IntRange,
    FloatRange,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic PRNG (xorshift64*): seeded, reproducible, no clock.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
    }

    // ------------------------------------------------------------------
    // Independent exact comparator: the oracle. Implemented by floor
    // comparison, sharing NOTHING with the decomposition the encoder and
    // Ord use — two independent derivations of the same law, cross-checked.
    // ------------------------------------------------------------------

    fn oracle_int_float(i: i64, f: f64) -> Ordering {
        if f.is_nan() {
            return Ordering::Less; // every number < NaN
        }
        if f == f64::INFINITY {
            return Ordering::Less;
        }
        if f == f64::NEG_INFINITY {
            return Ordering::Greater;
        }
        if f >= 9_223_372_036_854_775_808.0 {
            return Ordering::Less; // f >= 2^63 > any i64
        }
        if f < -9_223_372_036_854_775_808.0 {
            return Ordering::Greater;
        }
        let fl = f.floor();
        let fi = fl as i64; // exact: fl ∈ [-2^63, 2^63)
        match i.cmp(&fi) {
            Ordering::Less => Ordering::Less,
            Ordering::Greater => Ordering::Greater,
            // i == floor(f): if f has a fraction, i < f; if equal reals,
            // the tie-break says Int < Float. Both ways: Less.
            Ordering::Equal => Ordering::Less,
        }
    }

    fn oracle_cmp(a: Num, b: Num) -> Ordering {
        match (a.0, b.0) {
            (Repr::Int(x), Repr::Int(y)) => x.cmp(&y),
            (Repr::Float(x), Repr::Float(y)) => match (x.is_nan(), y.is_nan()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => x.partial_cmp(&y).expect("no NaN here"),
            },
            (Repr::Int(x), Repr::Float(y)) => oracle_int_float(x, y),
            (Repr::Float(x), Repr::Int(y)) => oracle_int_float(y, x).reverse(),
        }
    }

    // ------------------------------------------------------------------
    // Corpus: every boundary that has ever broken a numeric encoding,
    // plus seeded randoms over the full bit space.
    // ------------------------------------------------------------------

    fn corpus() -> Vec<Num> {
        let mut c = Vec::new();
        for i in [
            0i64,
            1,
            -1,
            2,
            -2,
            9,
            10,
            i64::MAX,
            i64::MIN,
            i64::MAX - 1,
            i64::MIN + 1,
            (1i64 << 52) - 1,
            1i64 << 52,
            (1i64 << 52) + 1,
            (1i64 << 53) - 1,
            1i64 << 53,
            (1i64 << 53) + 1,
            (1i64 << 62) + 12345,
            -((1i64 << 53) + 1),
        ] {
            c.push(Num::int(i));
        }
        for f in [
            0.0f64,
            -0.0,
            1.0,
            -1.0,
            1.5,
            -1.5,
            0.5,
            -0.5,
            2.0,
            f64::MAX,
            f64::MIN,
            f64::MIN_POSITIVE,
            5e-324, // smallest subnormal
            f64::MIN_POSITIVE / 2.0,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NAN,
            9_007_199_254_740_992.0,      // 2^53
            9_007_199_254_740_994.0,      // 2^53 + 2
            9_223_372_036_854_775_808.0,  // 2^63
            -9_223_372_036_854_775_808.0, // -2^63
            1e308,
            -1e308,
            1e-300,
            std::f64::consts::PI,
        ] {
            c.push(Num::float(f));
        }
        c
    }

    fn extend_random(c: &mut Vec<Num>, n: usize, seed: u64) {
        let mut rng = Rng(seed);
        for _ in 0..n {
            if rng.next().is_multiple_of(2) {
                c.push(Num::int(rng.next() as i64));
            } else {
                c.push(Num::float(f64::from_bits(rng.next())));
            }
        }
    }

    fn key(n: Num) -> Vec<u8> {
        let mut v = Vec::new();
        n.encode_key(&mut v);
        v
    }

    // ------------------------------------------------------------------
    // Laws.
    // ------------------------------------------------------------------

    /// Ord (the decomposition path) agrees with the oracle (the floor
    /// path) on every pair.
    #[test]
    fn law_semantic_order_matches_independent_oracle() {
        let mut c = corpus();
        extend_random(&mut c, 400, 0xA11CE);
        for &a in &c {
            for &b in &c {
                assert_eq!(
                    a.cmp(&b),
                    oracle_cmp(a, b),
                    "cmp diverged from oracle for {a:?} vs {b:?}"
                );
            }
        }
    }

    /// Key byte order equals semantic order on every pair.
    #[test]
    fn law_key_order_embeds_semantic_order() {
        let mut c = corpus();
        extend_random(&mut c, 400, 0xB0B);
        let keys: Vec<Vec<u8>> = c.iter().map(|&n| key(n)).collect();
        for i in 0..c.len() {
            for j in 0..c.len() {
                assert_eq!(
                    keys[i].cmp(&keys[j]),
                    c[i].cmp(&c[j]),
                    "key order diverged for {:?} vs {:?}",
                    c[i],
                    c[j]
                );
            }
        }
    }

    /// Total round-trip: identity-exact (repr and bits), with the
    /// normalizations applied at construction.
    #[test]
    fn law_round_trip_total() {
        let mut c = corpus();
        extend_random(&mut c, 4000, 0x5EED);
        for &n in &c {
            let k = key(n);
            let (back, used) = Num::decode_key(&k).expect("decode own encoding");
            assert_eq!(used, k.len());
            assert_eq!(back, n, "round-trip changed identity for {n:?}");
            assert_eq!(back.repr_byte(), n.repr_byte(), "repr changed for {n:?}");
        }
    }

    /// The numeric authority: ties equal, exactness beyond 2^53, named
    /// apart from the storage order.
    #[test]
    fn numeric_comparison_is_value_order_with_ties_equal() {
        assert_eq!(Num::int(1).cmp_numeric(Num::float(1.0)), Ordering::Equal);
        assert_eq!(Num::float(1.0).cmp_numeric(Num::int(1)), Ordering::Equal);
        assert!(Num::int(1).eq_numeric(Num::float(1.0)));
        assert_eq!(Num::int(1).cmp_numeric(Num::float(1.5)), Ordering::Less);
        assert_eq!(Num::float(2.5).cmp_numeric(Num::int(2)), Ordering::Greater);
        // Beyond 2^53: floats cannot represent the int; NOT equal.
        assert_ne!(
            Num::int((1 << 53) + 1).cmp_numeric(Num::float(9_007_199_254_740_992.0)),
            Ordering::Equal
        );
        // The storage order keeps its tie-break; numeric drops it. Both
        // named, both true.
        assert_eq!(Num::int(1).cmp(&Num::float(1.0)), Ordering::Less);
        // Differential vs the oracle over the corpus: equal reals are the
        // ONLY places the two authorities differ.
        let mut c = corpus();
        extend_random(&mut c, 300, 0xACE);
        for &a in &c {
            for &b in &c {
                let num = a.cmp_numeric(b);
                let sto = a.cmp(&b);
                if num != sto {
                    assert_eq!(num, Ordering::Equal, "authorities may differ only on ties");
                }
            }
        }
    }

    /// The identity law's pinned edges.
    #[test]
    fn identity_law_edges() {
        // -0.0 collapses at construction.
        assert_eq!(Num::float(-0.0), Num::float(0.0));
        assert_eq!(key(Num::float(-0.0)), key(Num::float(0.0)));
        assert_eq!(
            Num::float(-0.0).as_float().unwrap().to_bits(),
            0.0f64.to_bits()
        );
        // All NaNs are one NaN, equal to itself, greatest.
        let weird_nan = f64::from_bits(0xFFF8_DEAD_BEEF_0001);
        assert_eq!(Num::float(weird_nan), Num::float(f64::NAN));
        assert_eq!(key(Num::float(weird_nan)), key(Num::float(f64::NAN)));
        assert!(Num::float(f64::NAN) > Num::float(f64::INFINITY));
        // Int and Float of equal real are adjacent, Int first, both signs.
        assert!(Num::int(1) < Num::float(1.0));
        assert!(Num::int(-1) < Num::float(-1.0));
        assert!(Num::int(0) < Num::float(0.0));
        assert!(Num::float(1.0) < Num::int(2));
        // Beyond 2^53: floats cannot represent every int; order stays exact.
        assert!(Num::int((1 << 53) + 1) > Num::float(9_007_199_254_740_992.0));
        assert!(Num::int(i64::MAX) < Num::float(9_223_372_036_854_775_808.0));
        assert!(Num::float(f64::INFINITY) > Num::int(i64::MAX));
        assert!(Num::float(f64::NEG_INFINITY) < Num::int(i64::MIN));
    }

    /// Format v1 golden vectors: these bytes are permanent. A failure here
    /// means the on-disk numeric key moved, which is forbidden.
    #[test]
    fn format_v1_golden_vectors() {
        let cases: [(Num, &str); 12] = [
            (Num::int(0), "0200"),
            (Num::float(0.0), "0201"),
            (Num::int(1), "03043980000000000000000000"),
            (Num::float(1.0), "03043980000000000000000001"),
            (Num::int(-1), "01fbc67fffffffffffffffff00"),
            (Num::float(-1.0), "01fbc67fffffffffffffffff01"),
            (Num::int(2), "03043a80000000000000000000"),
            (Num::float(0.5), "03043880000000000000000001"),
            (Num::int(i64::MAX), "030477fffffffffffffffe0000"),
            (Num::int(i64::MIN), "01fb877fffffffffffffffff00"),
            (Num::float(f64::INFINITY), "03ffffffffffffffffffffff01"),
            (Num::float(f64::NAN), "0401"),
        ];
        for (n, hex) in cases {
            let k = key(n);
            let got: String = k.iter().map(|b| format!("{b:02x}")).collect();
            assert_eq!(got, hex, "golden vector moved for {n:?}");
        }
    }

    /// Decode totality: arbitrary bytes are Ok or a typed error, never a
    /// panic; truncations of valid keys are errors.
    #[test]
    fn decode_is_total() {
        let mut rng = Rng(0xF00D);
        for _ in 0..20_000 {
            let len = (rng.next() % 16) as usize;
            let bytes: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
            let _ = Num::decode_key(&bytes); // must not panic
        }
        let k = key(Num::int(12345));
        for cut in 0..k.len() {
            assert!(
                Num::decode_key(&k[..cut]).is_err(),
                "truncation at {cut} accepted"
            );
        }
    }
}
