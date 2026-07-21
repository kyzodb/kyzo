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
//! Seats: **14a** (one law + identity-first float divergence), **98**
//! (future Decimal/BigInt tag encoding — not this layout).
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
//! - **Identity-first (law, not a defect):** `-0.0` normalizes to `+0.0`
//!   at construction; all NaN bit patterns collapse to one canonical NaN
//!   (`Float`, equal to itself, **greatest** — above `+∞`). Float order
//!   authority is IEEE 754-2019 totalOrder; Kyzo’s ruled divergence from
//!   strict totalOrder *payload fidelity* is this collapse — **prior art
//!   matching CockroachDB/TiDB** (decisions.md seat 14a, locked again in
//!   seat 98). Do **not** “fix” toward preserving distinct NaN payloads
//!   or signed zero as distinct stored identities: that splits equal
//!   things under dedup forever.
//! - `±∞` order as the finite extremes' neighbors.
//!
//! Consequence: equivalence = dedup identity = code identity, and numeric
//! *order* crosses representations while numeric *identity* does not.
//!
//! Two durable laws riding on this identity:
//! - `Int(1) != Float(1.0)` is **query-layer semantics forever**, not an
//!   encoding detail — the expression layer's oracle differentials verify
//!   that the semantics agree across all consumers.
//! - The v1 numeric domain is **closed**: exactly `i64 ∪ f64`. This
//!   fixed-width key is permanent. Decimal / BigInt, when they earn Tag
//!   membership, are **new kinds** under seat 98 — SQLite4/CockroachDB
//!   `E(exp)` + base-100 self-terminating-mantissa (ELEN §7 production
//!   form) — **never** an extension or widen of this `Num` layout.
//!   Named blocker until then: those Tag variants are not in the Tag
//!   enum yet (research-open encode; do not invent a Decimal type here).
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
/// [`Num::int`] / [`Num::float`]; `Ord` is the **storage** law (exact real
/// order, `Int < Float` on equal reals, NaN greatest) — lawful as a trait
/// here because `Num` is fully inline: no deref, no context. Query-semantic
/// numeric order (ties equal: `1 == 1.0`) lives on [`NumericOrd`], never
/// as a second method on this type.
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

    /// Construct a float under the identity-first law (seats 14a / 98):
    /// `-0.0 → +0.0`, any NaN → the canonical NaN. Not payload-faithful
    /// IEEE totalOrder — Cockroach/TiDB prior art; do not "restore" distinct
    /// NaN payloads or signed zero.
    pub fn float(v: f64) -> Num {
        if v.is_nan() {
            return Num(Repr::Float(f64::from_bits(CANON_NAN_BITS)));
        }
        if v == 0.0 {
            return Num(Repr::Float(0.0));
        }
        Num(Repr::Float(v))
    }

    /// The numeric value as f64: ints promote by cast. A NUMERIC read
    /// for math kernels, never an identity claim (beyond 2^53 the cast
    /// rounds).
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

    /// The integer VALUE via numeric coercion: an int, or an integral float
    /// inside i64's exact range. This is DISTINCT from [`Num::as_int`],
    /// which reads only the `Int` representation — coercion (e.g. a `3.0`
    /// literal written into an `Int` column) goes through here.
    ///
    /// The bound is the exact power of two, not `i64::MAX as f64`: the true
    /// max (2^63 - 1) is not exactly representable in f64, so `i64::MAX as
    /// f64` rounds UP to 2^63 and would admit 2^63 itself — one past the
    /// boundary, which then saturates to `i64::MAX` on cast, silently
    /// fabricating a different index key.
    pub fn to_int_coerced(self) -> Option<i64> {
        match self.0 {
            Repr::Int(v) => Some(v),
            Repr::Float(f) => {
                const I64_MAX_BOUND_EXCLUSIVE: f64 = 9223372036854775808.0; // 2^63
                if f.round() == f && f >= i64::MIN as f64 && f < I64_MAX_BOUND_EXCLUSIVE {
                    Some(f as i64)
                } else {
                    None
                }
            }
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
            Repr::Int(_) | Repr::Float(_) => {
                let (neg, mag) = self.sign_magnitude();
                out.push(if neg { CLASS_NEG } else { CLASS_POS });
                let start = out.len();
                match mag {
                    Magnitude::Inf => {
                        out.extend_from_slice(&EXP_INF.to_be_bytes());
                        out.extend_from_slice(&[0xFF; 9]);
                    }
                    Magnitude::Finite { exp_key, frac72 } => {
                        // exp_key was proven at Magnitude construction
                        // (E ∈ [-1073, 1024] ⇒ biased ∈ [7, 2104] ⊂ u16).
                        debug_assert!(exp_key < EXP_INF);
                        out.extend_from_slice(&exp_key.to_be_bytes());
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
            // INVARIANT(num_twos_complement): magnitude already range-checked; wrap-neg is two's-complement encode.
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
                (neg, Magnitude::finite(e, frac72))
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
                    (neg, Magnitude::finite(e, frac72))
                } else {
                    // Subnormal: frac52 × 2^-1074.
                    let bl = 64 - frac52.leading_zeros();
                    let e = bl as i32 - 1074;
                    let frac72 = (frac52 as u128) << (72 - bl);
                    (neg, Magnitude::finite(e, frac72))
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
    /// Biased exponent already in key form: `E + EXP_OFFSET` as `u16`.
    /// Constructed only via [`Magnitude::finite`], which proves the range
    /// so [`Num::encode_key`] never re-checks with expect.
    Finite {
        exp_key: u16,
        frac72: u128,
    },
    Inf,
}

impl Magnitude {
    /// Finite magnitude door: `e ∈ [-1073, 1024]` from int/float bit math
    /// ⇒ biased exponent fits `u16` and sits strictly below [`EXP_INF`].
    fn finite(e: i32, frac72: u128) -> Self {
        // INVARIANT(NumExpBias): e ∈ [-1073, 1024] by the finite-magnitude door;
        // wrap adds EXP_OFFSET into the proven u16 biased-exponent range.
        let biased = e.wrapping_add(EXP_OFFSET);
        debug_assert!((7..=2104).contains(&biased));
        Magnitude::Finite {
            exp_key: biased as u16,
            frac72,
        }
    }
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
    /// Storage / identity order: exact real order, `Int < Float` on equal
    /// reals, NaN greatest and equal to itself. Matches the order-preserving
    /// key. Query-semantic numeric order is [`NumericOrd`].
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
                        Magnitude::Finite {
                            exp_key: ea,
                            frac72: fa,
                        },
                        Magnitude::Finite {
                            exp_key: eb,
                            frac72: fb,
                        },
                    ) => ea.cmp(&eb).then(fa.cmp(&fb)),
                };
                let real = if neg { mag.reverse() } else { mag };
                real.then(self.repr_byte().cmp(&other.repr_byte()))
            }
        }
    }
}

/// Exact real-value order for query/expression semantics.
///
/// Law: real-number order with ties EQUAL — `Int(1)` and `Float(1.0)`
/// compare `Equal` here. Distinct from [`Num`]'s storage `Ord`, which
/// places `Int < Float` on equal reals. Two total orders = two types;
/// expression compare/eq/min/max must go through this newtype, never
/// through a second method on [`Num`].
///
/// Private field: the only mint is [`NumericOrd::of`].
#[derive(Clone, Copy, Debug)]
pub struct NumericOrd(Num);

impl NumericOrd {
    /// Wrap a [`Num`] for query-semantic numeric order.
    #[inline]
    pub const fn of(n: Num) -> NumericOrd {
        NumericOrd(n)
    }

    /// The wrapped number (read-only; construction stays at [`of`]).
    #[inline]
    pub const fn get(self) -> Num {
        self.0
    }
}

impl PartialEq for NumericOrd {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for NumericOrd {}

impl PartialOrd for NumericOrd {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for NumericOrd {
    fn cmp(&self, other: &Self) -> Ordering {
        // Storage order is (real value, repr tie-break); stripping the
        // tie-break yields the numeric order exactly.
        match self.0.cmp(&other.0) {
            Ordering::Equal => Ordering::Equal,
            o @ Ordering::Less | o @ Ordering::Greater => {
                if self.0.repr_byte() != other.0.repr_byte() {
                    let same = matches!(
                        (self.0.as_int(), other.0.as_float()),
                        (Some(i), Some(f)) if int_float_eq(i, f)
                    ) || matches!(
                        (self.0.as_float(), other.0.as_int()),
                        (Some(f), Some(i)) if int_float_eq(i, f)
                    );
                    if same { Ordering::Equal } else { o }
                } else {
                    o
                }
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

    // ---- GUARDIAN ONE-LAW HUNT: independent true-numeric-order oracle ----
    // The edge-corpus one-law test checks byte order == Num::cmp, and its
    // "semantic" leg delegates to Num::cmp -- so nothing independently checks
    // Num::cmp against TRUE numeric order. encode_key AND Num::cmp both derive
    // from sign_magnitude(), so a magnitude bug at the int/float boundary or a
    // subnormal would agree across byte-order, Num::cmp, and the semantic leg
    // and stay green. This oracle computes the storage order from exact reals
    // WITHOUT sign_magnitude and drives the boundary cases the corpus omits.
    #[derive(Clone, Copy, Debug)]
    enum HuntV {
        I(i64),
        F(f64),
    }

    fn hunt_num(v: HuntV) -> Num {
        match v {
            HuntV::I(i) => Num::int(i),
            HuntV::F(f) => Num::float(f),
        }
    }

    fn hunt_enc(v: HuntV) -> Vec<u8> {
        let mut o = Vec::new();
        hunt_num(v).encode_key(&mut o);
        o
    }

    fn hunt_is_nan(v: HuntV) -> bool {
        matches!(v, HuntV::F(f) if f.is_nan())
    }

    /// Exact compare i64 vs (finite or infinite, non-NaN) f64.
    fn hunt_cmp_i_f(i: i64, f: f64) -> Ordering {
        if f.is_infinite() {
            return if f > 0.0 {
                Ordering::Less
            } else {
                Ordering::Greater
            };
        }
        if f >= 9223372036854775808.0 {
            return Ordering::Less; // f >= 2^63 > i64::MAX
        }
        if f < -9223372036854775808.0 {
            return Ordering::Greater; // f < -2^63 <= i64::MIN
        }
        let fl = f.floor();
        let fli = fl as i64; // exact: fl integral, in [-2^63, 2^63)
        match (i as i128).cmp(&(fli as i128)) {
            Ordering::Less => Ordering::Less,
            Ordering::Greater => Ordering::Greater,
            Ordering::Equal => {
                if f == fl {
                    Ordering::Equal
                } else {
                    Ordering::Less // floor(f) < f, so i == floor < f
                }
            }
        }
    }

    /// Independent STORAGE order: exact real order, NaN greatest and equal to
    /// itself, ties broken Int < Float (REPR_INT < REPR_FLOAT). -0.0 == +0.0.
    fn hunt_true_order(a: HuntV, b: HuntV) -> Ordering {
        match (hunt_is_nan(a), hunt_is_nan(b)) {
            (true, true) => return Ordering::Equal,
            (true, false) => return Ordering::Greater,
            (false, true) => return Ordering::Less,
            (false, false) => {}
        }
        let real = match (a, b) {
            (HuntV::I(x), HuntV::I(y)) => x.cmp(&y),
            (HuntV::F(x), HuntV::F(y)) => x.partial_cmp(&y).expect("no NaN in this arm"),
            (HuntV::I(x), HuntV::F(y)) => hunt_cmp_i_f(x, y),
            (HuntV::F(x), HuntV::I(y)) => hunt_cmp_i_f(y, x).reverse(),
        };
        if real != Ordering::Equal {
            return real;
        }
        let rank = |v: HuntV| matches!(v, HuntV::F(_)) as u8;
        rank(a).cmp(&rank(b))
    }

    #[test]
    fn one_law_holds_at_int_float_boundary_and_ugly_floats() {
        let corpus = [
            HuntV::I(0),
            HuntV::F(0.0),
            HuntV::F(-0.0),
            HuntV::I(1),
            HuntV::F(1.0),
            HuntV::I(-1),
            HuntV::F(-1.0),
            HuntV::I(i64::MAX),
            HuntV::I(i64::MIN),
            HuntV::F(9223372036854775808.0),  // 2^63 == i64::MAX + 1
            HuntV::F(9223372036854774784.0),  // largest f64 strictly below 2^63
            HuntV::F(-9223372036854775808.0), // -2^63 == i64::MIN
            HuntV::I(9007199254740993),       // 2^53 + 1 (not f64-representable)
            HuntV::F(9007199254740992.0),     // 2^53
            HuntV::F(9007199254740994.0),     // 2^53 + 2
            HuntV::F(f64::from_bits(1)),      // smallest positive subnormal
            HuntV::F(-f64::from_bits(1)),     // smallest negative subnormal
            HuntV::F(f64::MIN_POSITIVE),
            HuntV::F(1e-300),
            HuntV::F(-1e-300),
            HuntV::F(f64::NEG_INFINITY),
            HuntV::F(f64::INFINITY),
            HuntV::F(f64::NAN),
            HuntV::F(f64::MIN),
            HuntV::F(f64::MAX),
        ];
        for &a in &corpus {
            for &b in &corpus {
                let byte = hunt_enc(a).cmp(&hunt_enc(b));
                let truth = hunt_true_order(a, b);
                assert_eq!(
                    byte, truth,
                    "ONE-LAW VIOLATION: byte order {byte:?} != true numeric order {truth:?} for {a:?} vs {b:?}"
                );
                assert_eq!(
                    hunt_num(a).cmp(&hunt_num(b)),
                    byte,
                    "Num::cmp != byte order for {a:?} vs {b:?}"
                );
            }
        }
    }

    /// Deterministic PRNG (xorshift64*): seeded, reproducible, no clock.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            // INVARIANT(xorshift_finalizer): xorshift* final mul is defined wrapping on u64.
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

    /// Query-semantic numeric order: ties equal, exact beyond 2^53, typed
    /// apart from storage order on [`Num`].
    #[test]
    fn numeric_ord_is_value_order_with_ties_equal() {
        assert_eq!(
            NumericOrd::of(Num::int(1)).cmp(&NumericOrd::of(Num::float(1.0))),
            Ordering::Equal
        );
        assert_eq!(
            NumericOrd::of(Num::float(1.0)).cmp(&NumericOrd::of(Num::int(1))),
            Ordering::Equal
        );
        assert_eq!(NumericOrd::of(Num::int(1)), NumericOrd::of(Num::float(1.0)));
        assert_eq!(
            NumericOrd::of(Num::int(1)).cmp(&NumericOrd::of(Num::float(1.5))),
            Ordering::Less
        );
        assert_eq!(
            NumericOrd::of(Num::float(2.5)).cmp(&NumericOrd::of(Num::int(2))),
            Ordering::Greater
        );
        // Beyond 2^53: floats cannot represent the int; NOT equal.
        assert_ne!(
            NumericOrd::of(Num::int((1 << 53) + 1))
                .cmp(&NumericOrd::of(Num::float(9_007_199_254_740_992.0))),
            Ordering::Equal
        );
        // Storage keeps its tie-break; NumericOrd drops it. Both named.
        assert_eq!(Num::int(1).cmp(&Num::float(1.0)), Ordering::Less);
        // Differential vs storage over the corpus: equal reals are the
        // ONLY places the two authorities differ.
        let mut c = corpus();
        extend_random(&mut c, 300, 0xACE);
        for &a in &c {
            for &b in &c {
                let num = NumericOrd::of(a).cmp(&NumericOrd::of(b));
                let sto = a.cmp(&b);
                if num != sto {
                    assert_eq!(num, Ordering::Equal, "authorities may differ only on ties");
                }
            }
        }
    }

    /// Identity-first pin (seats 14a / 98): NaN-collapse and `-0` normalize
    /// are construction law matching Cockroach/TiDB — not a hole to fill
    /// toward strict IEEE 754-2019 totalOrder payload fidelity.
    #[test]
    fn identity_law_edges() {
        // -0.0 collapses at construction (not a distinct stored identity).
        assert_eq!(Num::float(-0.0), Num::float(0.0));
        assert_eq!(key(Num::float(-0.0)), key(Num::float(0.0)));
        assert_eq!(
            Num::float(-0.0).as_float().unwrap().to_bits(),
            0.0f64.to_bits()
        );
        // All NaN payloads are one canonical NaN, equal to itself, greatest.
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

    /// `Num`'s order is total: `PartialOrd` never returns `None` — the
    /// IEEE NaN hole is closed at construction (one canonical NaN).
    #[test]
    fn law_num_partial_ord_is_total_no_nan_hole() {
        let mut c = corpus();
        extend_random(&mut c, 200, 0xBAD_70_E1);
        // Inject many raw NaN bit patterns; construction collapses them.
        for bits in [
            0x7FF8_0000_0000_0000u64,
            0xFFF8_0000_0000_0001,
            0x7FF0_DEAD_BEEF_0001,
            0xFFFF_FFFF_FFFF_FFFF,
        ] {
            c.push(Num::float(f64::from_bits(bits)));
        }
        for &a in &c {
            for &b in &c {
                let p = a.partial_cmp(&b);
                assert!(p.is_some(), "Num PartialOrd hole (NaN?): {a:?} vs {b:?}");
                assert_eq!(p, Some(a.cmp(&b)));
            }
        }
        // Canonical NaN is greatest and equal to every other NaN mint.
        let nan = Num::float(f64::NAN);
        assert_eq!(nan.partial_cmp(&nan), Some(Ordering::Equal));
        assert_eq!(
            nan.partial_cmp(&Num::float(f64::INFINITY)),
            Some(Ordering::Greater)
        );
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

    /// Every [`NumDecodeError`] variant is reachable through
    /// [`Num::decode_key`] — not merely "is_err" against random bytes.
    #[test]
    fn num_decode_error_variants_deliberately_constructed() {
        assert_eq!(Num::decode_key(&[]), Err(NumDecodeError::Truncated));
        assert_eq!(Num::decode_key(&[0x02]), Err(NumDecodeError::Truncated));
        assert_eq!(Num::decode_key(&[0x00]), Err(NumDecodeError::BadClass));
        assert_eq!(Num::decode_key(&[0x02, 0x7F]), Err(NumDecodeError::BadRepr));
        assert_eq!(Num::decode_key(&[0x04, 0x00]), Err(NumDecodeError::BadRepr));

        // POS + INF exp + wrong fraction → BadInfinity
        let mut bad_inf = vec![0x03, 0xFF, 0xFF];
        bad_inf.extend_from_slice(&[0x00; 9]); // frac not all-ones
        bad_inf.push(0x01); // float repr
        assert_eq!(Num::decode_key(&bad_inf), Err(NumDecodeError::BadInfinity));

        // POS + finite exp + leading bit clear → Denormalized
        let mut denorm = vec![0x03, 0x04, 0x38]; // exp for ~0.5 range
        denorm.extend_from_slice(&[0x00; 9]); // leading 1 missing
        denorm.push(0x01);
        assert_eq!(Num::decode_key(&denorm), Err(NumDecodeError::Denormalized));

        // Int labeled with e=0 (out of 1..=64) → IntRange via rebuild after
        // setting a leading-1 fraction at an illegal exponent.
        // Use a lawful float key and flip the repr byte to Int with an
        // exponent that floats use but ints refuse (e.g. e for 0.5 → e=0
        // after offset decode is negative / out of int range).
        let mut half = key(Num::float(0.5));
        let last = half.len() - 1;
        half[last] = 0x00; // force Int repr on a fractional magnitude
        assert_eq!(Num::decode_key(&half), Err(NumDecodeError::IntRange));

        // Float with e > 1024 (beyond finite): take INF exp with float
        // repr but truncate fraction so BadInfinity already covers INF;
        // FloatRange: subnormal path with bl out of 1..=52.
        // Craft: POS, exp = 0 (stored) → e = -1080, subnormal bl = -6 → FloatRange.
        let mut far = vec![0x03, 0x00, 0x00];
        far.push(0x80); // leading 1 at bit 71
        far.extend_from_slice(&[0x00; 8]);
        far.push(0x01);
        assert_eq!(Num::decode_key(&far), Err(NumDecodeError::FloatRange));
    }
}
