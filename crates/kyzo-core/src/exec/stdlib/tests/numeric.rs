/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Re-homed domain tables from data/tests/functions.rs.
use crate::exec::stdlib::numeric::*;
use kyzo_model::value::{DataValue, Vector};
use std::f64::consts::{E, PI};

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-5
}

#[test]
fn test_add() {
    assert_eq!(op_add(&[]).unwrap(), DataValue::from(0));
    assert_eq!(op_add(&[DataValue::from(1)]).unwrap(), DataValue::from(1));
    assert_eq!(
        op_add(&[DataValue::from(1), DataValue::from(2)]).unwrap(),
        DataValue::from(3)
    );
    assert_eq!(
        op_add(&[DataValue::from(1), DataValue::from(2.5)]).unwrap(),
        DataValue::from(3.5)
    );
    assert_eq!(
        op_add(&[DataValue::from(1.5), DataValue::from(2.5)]).unwrap(),
        DataValue::from(4.0)
    );
    // Boundary: i64::MAX stays exact right up to the edge, then errors
    // rather than wrapping to i64::MIN (release builds) or panicking
    // (debug builds).
    assert_eq!(
        op_add(&[DataValue::from(i64::MAX - 1), DataValue::from(1)]).unwrap(),
        DataValue::from(i64::MAX)
    );
    assert!(op_add(&[DataValue::from(i64::MAX), DataValue::from(1)]).is_err());
    assert!(op_add(&[DataValue::from(i64::MIN), DataValue::from(-1)]).is_err());
}

#[test]
fn test_sub() {
    assert_eq!(
        op_sub(&[DataValue::from(1), DataValue::from(2)]).unwrap(),
        DataValue::from(-1)
    );
    assert_eq!(
        op_sub(&[DataValue::from(1), DataValue::from(2.5)]).unwrap(),
        DataValue::from(-1.5)
    );
    assert_eq!(
        op_sub(&[DataValue::from(1.5), DataValue::from(2.5)]).unwrap(),
        DataValue::from(-1.0)
    );
    // Boundary: i64::MIN - 1 (and i64::MAX - (-1)) overflow and error.
    assert_eq!(
        op_sub(&[DataValue::from(i64::MIN + 1), DataValue::from(1)]).unwrap(),
        DataValue::from(i64::MIN)
    );
    assert!(op_sub(&[DataValue::from(i64::MIN), DataValue::from(1)]).is_err());
    assert!(op_sub(&[DataValue::from(i64::MAX), DataValue::from(-1)]).is_err());
}

#[test]
fn test_mul() {
    assert_eq!(op_mul(&[]).unwrap(), DataValue::from(1));
    assert_eq!(
        op_mul(&[DataValue::from(2), DataValue::from(3)]).unwrap(),
        DataValue::from(6)
    );
    assert_eq!(
        op_mul(&[DataValue::from(0.5), DataValue::from(0.25)]).unwrap(),
        DataValue::from(0.125)
    );
    assert_eq!(
        op_mul(&[DataValue::from(0.5), DataValue::from(3)]).unwrap(),
        DataValue::from(1.5)
    );
    // Boundary: i64::MAX itself stays exact; doubling it overflows.
    assert_eq!(
        op_mul(&[DataValue::from(i64::MAX), DataValue::from(1)]).unwrap(),
        DataValue::from(i64::MAX)
    );
    assert!(op_mul(&[DataValue::from(i64::MAX), DataValue::from(2)]).is_err());
    // Regression for fuzz artifact crash-f1ef21a6c4f99a02f719c5bde2689bb158df629f:
    // parse-time constant folding of this literal product panicked with
    // "attempt to multiply with overflow" in debug builds and silently
    // wrapped in release builds; it must now be a clean typed error.
    assert!(
        op_mul(&[
            DataValue::from(2222222000_i64),
            DataValue::from(867076028303_i64)
        ])
        .is_err()
    );
}

fn f64_vec(xs: &[f64]) -> DataValue {
    DataValue::Vector(Vector::try_new(xs.to_vec()).unwrap())
}

// Regression for the upstream bug where multiplying three or more vectors
// recursed into vector *addition* for the prefix: `v1 * v2 * v3` computed
// `(v1 + v2) * v3`. Multiplication must multiply.
#[test]
fn test_mul_vecs_multiplies() {
    assert_eq!(
        op_mul(&[f64_vec(&[2., 3.]), f64_vec(&[4., 5.])]).unwrap(),
        f64_vec(&[8., 15.])
    );
    // Three arguments: the buggy version yields [36., 56.] here.
    assert_eq!(
        op_mul(&[f64_vec(&[2., 3.]), f64_vec(&[4., 5.]), f64_vec(&[6., 7.])]).unwrap(),
        f64_vec(&[48., 105.])
    );
    // Scalars broadcast over vectors.
    assert_eq!(
        op_mul(&[f64_vec(&[2., 3.]), DataValue::from(10.)]).unwrap(),
        f64_vec(&[20., 30.])
    );
}

#[test]
fn test_div() {
    assert_eq!(
        op_div(&[DataValue::from(1), DataValue::from(1)]).unwrap(),
        DataValue::from(1.0)
    );
    assert_eq!(
        op_div(&[DataValue::from(1), DataValue::from(2)]).unwrap(),
        DataValue::from(0.5)
    );
    assert_eq!(
        op_div(&[DataValue::from(7.0), DataValue::from(0.5)]).unwrap(),
        DataValue::from(14.0)
    );
}

/// Division and modulo by zero must both raise the same typed refusal,
/// integer or float, never a silent `Infinity`/`NaN`. `1 / 0` and `1 % 0`
/// used to disagree (`div` float-promoted to `Infinity`, `mod` alone
/// refused); this locks the two to one consistent typed error.
#[test]
fn test_div_mod_by_zero_typed_error() {
    for res in [
        op_div(&[DataValue::from(1), DataValue::from(0)]),
        op_div(&[DataValue::from(1.0), DataValue::from(0.0)]),
        op_div(&[DataValue::from(0.0), DataValue::from(0.0)]),
        op_div(&[DataValue::from(-1), DataValue::from(0)]),
        op_mod(&[DataValue::from(1), DataValue::from(0)]),
    ] {
        let err = res.expect_err("division/modulo by zero must be a typed Err, not Ok");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("division_by_zero"),
            "expected the division-by-zero diagnostic code, got: {msg}"
        );
    }
}

#[test]
fn test_max_min() {
    assert_eq!(op_max(&[DataValue::from(1)]).unwrap(), DataValue::from(1));
    assert_eq!(
        op_max(&[
            DataValue::from(1),
            DataValue::from(2),
            DataValue::from(3),
            DataValue::from(4)
        ])
        .unwrap(),
        DataValue::from(4)
    );
    assert_eq!(
        op_max(&[
            DataValue::from(1.0),
            DataValue::from(2),
            DataValue::from(3),
            DataValue::from(4)
        ])
        .unwrap(),
        DataValue::from(4)
    );
    assert_eq!(
        op_max(&[
            DataValue::from(1),
            DataValue::from(2),
            DataValue::from(3),
            DataValue::from(4.0)
        ])
        .unwrap(),
        DataValue::from(4.0)
    );
    assert!(op_max(&[DataValue::from(true)]).is_err());

    assert_eq!(op_min(&[DataValue::from(1)]).unwrap(), DataValue::from(1));
    assert_eq!(
        op_min(&[
            DataValue::from(1),
            DataValue::from(2),
            DataValue::from(3),
            DataValue::from(4)
        ])
        .unwrap(),
        DataValue::from(1)
    );
    assert_eq!(
        op_min(&[
            DataValue::from(1.0),
            DataValue::from(2),
            DataValue::from(3),
            DataValue::from(4)
        ])
        .unwrap(),
        DataValue::from(1.0)
    );
    assert_eq!(
        op_min(&[
            DataValue::from(1),
            DataValue::from(2),
            DataValue::from(3),
            DataValue::from(4.0)
        ])
        .unwrap(),
        DataValue::from(1)
    );
    assert!(op_max(&[DataValue::from(true)]).is_err());
}

#[test]
fn test_minus() {
    assert_eq!(
        op_minus(&[DataValue::from(-1)]).unwrap(),
        DataValue::from(1)
    );
    assert_eq!(
        op_minus(&[DataValue::from(1)]).unwrap(),
        DataValue::from(-1)
    );
    assert_eq!(
        op_minus(&[DataValue::from(f64::INFINITY)]).unwrap(),
        DataValue::from(f64::NEG_INFINITY)
    );
    assert_eq!(
        op_minus(&[DataValue::from(f64::NEG_INFINITY)]).unwrap(),
        DataValue::from(f64::INFINITY)
    );
    // Boundary: i64::MIN has no positive i64 counterpart (i64::MAX is one
    // short of |i64::MIN|), so negating it errors instead of wrapping.
    assert_eq!(
        op_minus(&[DataValue::from(i64::MIN + 1)]).unwrap(),
        DataValue::from(i64::MAX)
    );
    assert!(op_minus(&[DataValue::from(i64::MIN)]).is_err());
}

#[test]
fn test_abs() {
    assert_eq!(op_abs(&[DataValue::from(-1)]).unwrap(), DataValue::from(1));
    assert_eq!(op_abs(&[DataValue::from(1)]).unwrap(), DataValue::from(1));
    assert_eq!(
        op_abs(&[DataValue::from(-1.5)]).unwrap(),
        DataValue::from(1.5)
    );
    // Boundary: same asymmetry as unary minus.
    assert_eq!(
        op_abs(&[DataValue::from(i64::MIN + 1)]).unwrap(),
        DataValue::from(i64::MAX)
    );
    assert!(op_abs(&[DataValue::from(i64::MIN)]).is_err());
}

#[test]
fn test_signum() {
    assert_eq!(
        op_signum(&[DataValue::from(0.1)]).unwrap(),
        DataValue::from(1)
    );
    assert_eq!(
        op_signum(&[DataValue::from(-0.1)]).unwrap(),
        DataValue::from(-1)
    );
    assert_eq!(
        op_signum(&[DataValue::from(0.0)]).unwrap(),
        DataValue::from(0)
    );
    // `-0.0` collapses to `+0.0` in the value plane (one canonical zero),
    // so its signum is `0`, not `-1`.
    assert_eq!(
        op_signum(&[DataValue::from(-0.0)]).unwrap(),
        DataValue::from(0)
    );
    assert_eq!(
        op_signum(&[DataValue::from(-3)]).unwrap(),
        DataValue::from(-1)
    );
    assert_eq!(
        op_signum(&[DataValue::from(f64::NEG_INFINITY)]).unwrap(),
        DataValue::from(-1)
    );
    assert!(
        op_signum(&[DataValue::from(f64::NAN)])
            .unwrap()
            .get_float()
            .unwrap()
            .is_nan()
    );
}

#[test]
fn test_floor_ceil() {
    assert_eq!(
        op_floor(&[DataValue::from(-1)]).unwrap(),
        DataValue::from(-1)
    );
    assert_eq!(
        op_floor(&[DataValue::from(-1.5)]).unwrap(),
        DataValue::from(-2.0)
    );
    assert_eq!(
        op_floor(&[DataValue::from(1.5)]).unwrap(),
        DataValue::from(1.0)
    );
    assert_eq!(
        op_ceil(&[DataValue::from(-1)]).unwrap(),
        DataValue::from(-1)
    );
    assert_eq!(
        op_ceil(&[DataValue::from(-1.5)]).unwrap(),
        DataValue::from(-1.0)
    );
    assert_eq!(
        op_ceil(&[DataValue::from(1.5)]).unwrap(),
        DataValue::from(2.0)
    );
}

#[test]
fn test_round() {
    assert_eq!(
        op_round(&[DataValue::from(0.6)]).unwrap(),
        DataValue::from(1.0)
    );
    assert_eq!(
        op_round(&[DataValue::from(0.5)]).unwrap(),
        DataValue::from(1.0)
    );
    assert_eq!(
        op_round(&[DataValue::from(1.5)]).unwrap(),
        DataValue::from(2.0)
    );
    assert_eq!(
        op_round(&[DataValue::from(-0.6)]).unwrap(),
        DataValue::from(-1.0)
    );
    assert_eq!(
        op_round(&[DataValue::from(-0.5)]).unwrap(),
        DataValue::from(-1.0)
    );
    assert_eq!(
        op_round(&[DataValue::from(-1.5)]).unwrap(),
        DataValue::from(-2.0)
    );
}

#[test]
fn test_exp() {
    let n = op_exp(&[DataValue::from(1)]).unwrap().get_float().unwrap();
    assert!(close(n, E));

    let n = op_exp(&[DataValue::from(50.1)])
        .unwrap()
        .get_float()
        .unwrap();
    assert!(close(n, 50.1_f64.exp()));
}

#[test]
fn test_exp2() {
    let n = op_exp2(&[DataValue::from(10.)])
        .unwrap()
        .get_float()
        .unwrap();
    assert_eq!(n, 1024.);
}

#[test]
fn test_ln() {
    assert_eq!(op_ln(&[DataValue::from(E)]).unwrap(), DataValue::from(1.0));
}

#[test]
fn test_log2() {
    assert_eq!(
        op_log2(&[DataValue::from(1024)]).unwrap(),
        DataValue::from(10.)
    );
}

#[test]
fn test_log10() {
    assert_eq!(
        op_log10(&[DataValue::from(1000)]).unwrap(),
        DataValue::from(3.0)
    );
}

#[test]
fn test_trig() {
    assert!(close(
        op_sin(&[DataValue::from(PI / 2.)])
            .unwrap()
            .get_float()
            .unwrap(),
        1.0
    ));
    assert!(close(
        op_cos(&[DataValue::from(PI / 2.)])
            .unwrap()
            .get_float()
            .unwrap(),
        0.0
    ));
    assert!(close(
        op_tan(&[DataValue::from(PI / 4.)])
            .unwrap()
            .get_float()
            .unwrap(),
        1.0
    ));
}

#[test]
fn test_inv_trig() {
    assert!(close(
        op_asin(&[DataValue::from(1.0)])
            .unwrap()
            .get_float()
            .unwrap(),
        PI / 2.
    ));
    assert!(close(
        op_acos(&[DataValue::from(0)]).unwrap().get_float().unwrap(),
        PI / 2.
    ));
    assert!(close(
        op_atan(&[DataValue::from(1)]).unwrap().get_float().unwrap(),
        PI / 4.
    ));
    assert!(close(
        op_atan2(&[DataValue::from(-1), DataValue::from(-1)])
            .unwrap()
            .get_float()
            .unwrap(),
        -3. * PI / 4.
    ));
}

/// The same silent-poison shape `DivisionByZero` fixed for `div`/`mod`:
/// every partial math op with a restricted domain now refuses an
/// out-of-domain input with a typed `domain_error`, never a quiet `NaN`
/// (or, at a domain boundary that diverges, a quiet infinity).
#[test]
fn test_math_domain_errors_typed() {
    for res in [
        op_sqrt(&[DataValue::from(-1)]),
        op_ln(&[DataValue::from(0)]),
        op_ln(&[DataValue::from(-1)]),
        op_log2(&[DataValue::from(-1)]),
        op_log10(&[DataValue::from(-1)]),
        op_asin(&[DataValue::from(2)]),
        op_acos(&[DataValue::from(2)]),
        op_acosh(&[DataValue::from(0)]),
        op_atanh(&[DataValue::from(2)]),
        // `atanh` also diverges to an infinity at either open-interval
        // boundary, exactly the poison shape `div`/`mod` were fixed for.
        op_atanh(&[DataValue::from(1)]),
        op_atanh(&[DataValue::from(-1)]),
        // `pow`: a negative base with a fractional exponent has no real
        // result, and a zero base with a negative exponent diverges to an
        // infinity (the same shape as a division by zero, expressed
        // through `pow`).
        op_pow(&[DataValue::from(-1.0), DataValue::from(0.5)]),
        op_pow(&[DataValue::from(0.0), DataValue::from(-1.0)]),
    ] {
        let err = res.expect_err("out-of-domain math input must be a typed Err, not Ok(NaN)");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("domain_error"),
            "expected the domain-error diagnostic code, got: {msg}"
        );
    }
}

/// The vector lane of the same ops must refuse an out-of-domain element
/// too, rather than poisoning the whole vector with a `NaN` at that
/// position.
#[test]
fn test_math_domain_errors_vector() {
    let has_negative = Vector::try_new(vec![1.0, -1.0, 4.0]).unwrap();
    assert!(op_sqrt(&[DataValue::Vector(has_negative)]).is_err());

    let has_out_of_range = Vector::try_new(vec![0.0f64, 2.0]).unwrap();
    assert!(op_asin(&[DataValue::Vector(has_out_of_range)]).is_err());

    let has_non_positive = Vector::try_new(vec![1.0, 0.0]).unwrap();
    assert!(op_ln(&[DataValue::Vector(has_non_positive)]).is_err());
}

/// Valid, in-domain inputs to the same ops still compute the correct
/// value — the domain guard must not reject anything it shouldn't.
#[test]
fn test_math_valid_inputs_unaffected() {
    assert_eq!(
        op_sqrt(&[DataValue::from(4)]).unwrap(),
        DataValue::from(2.0)
    );
    assert_eq!(op_ln(&[DataValue::from(1)]).unwrap(), DataValue::from(0.0));
    assert_eq!(
        op_asin(&[DataValue::from(0)]).unwrap(),
        DataValue::from(0.0)
    );
    assert_eq!(
        op_acos(&[DataValue::from(1)]).unwrap(),
        DataValue::from(0.0)
    );
    // Boundary lock: the exact edges the domain guards must NOT refuse, so a
    // later tightening of a guard (e.g. `<` slipping to `<=`, or a closed
    // interval narrowed to open) is caught. sqrt/asin/acos domains are
    // closed; acosh is closed at 1; atanh is open, so a value just inside it
    // must still compute; a negative base with an integral exponent is a real
    // power `pow` must not refuse.
    assert_eq!(
        op_sqrt(&[DataValue::from(0)]).unwrap(),
        DataValue::from(0.0)
    );
    assert_eq!(
        op_acosh(&[DataValue::from(1)]).unwrap(),
        DataValue::from(0.0)
    );
    assert!(
        op_asin(&[DataValue::from(1)]).is_ok(),
        "asin(+1) is in domain"
    );
    assert!(
        op_asin(&[DataValue::from(-1)]).is_ok(),
        "asin(-1) is in domain"
    );
    assert!(
        op_acos(&[DataValue::from(-1)]).is_ok(),
        "acos(-1) is in domain"
    );
    assert!(
        op_atanh(&[DataValue::from(0.5)]).is_ok(),
        "atanh(0.5) is strictly inside the open interval"
    );
    assert_eq!(
        op_pow(&[DataValue::from(-2.0), DataValue::from(3.0)]).unwrap(),
        DataValue::from(-8.0),
        "a negative base with an integral exponent is a real power"
    );
}

/// The same silent-NaN class the partial scalar ops were swept for also
/// lived in two vector-distance ops: cosine distance and L2 normalization
/// both divide by a vector norm, so a zero vector yielded `0/0 = NaN`
/// (or an infinity) with no diagnostic. Both now refuse with the same typed
/// `domain_error`, on both the F32 and F64 lanes, while non-degenerate
/// vectors still compute.
#[test]
fn test_pow() {
    assert_eq!(
        op_pow(&[DataValue::from(2), DataValue::from(10)]).unwrap(),
        DataValue::from(1024.0)
    );
    // `pow` always promotes integer operands to f64 (unlike `+`/`-`/`*`,
    // which stay exact `i64` until they overflow it): a result or exponent
    // far past i64's range saturates to infinity rather than overflowing
    // or panicking, the same as any other float op.
    let huge = op_pow(&[DataValue::from(i64::MAX), DataValue::from(i64::MAX)])
        .unwrap()
        .get_float()
        .unwrap();
    assert!(huge.is_infinite());
}

#[test]
fn test_mod() {
    assert_eq!(
        op_mod(&[DataValue::from(-10), DataValue::from(7)]).unwrap(),
        DataValue::from(-3)
    );
    // A zero divisor is refused the same way regardless of which operand
    // (or both) is float: `mod` never silently promotes to a NaN any more
    // than `div` may silently promote to `Infinity` — same typed error,
    // integer or float divisor.
    assert!(op_mod(&[DataValue::from(5), DataValue::from(0.)]).is_err());
    assert!(op_mod(&[DataValue::from(5.), DataValue::from(0.)]).is_err());
    assert!(op_mod(&[DataValue::from(5.), DataValue::from(0)]).is_err());
    assert!(op_mod(&[DataValue::from(5), DataValue::from(0)]).is_err());
    // Boundary: i64::MIN % -1 is the one nonzero divisor `Rem` still can't
    // service (the implied i64::MIN / -1 doesn't fit in i64) — distinct
    // from, and in addition to, the zero-divisor case above.
    assert!(op_mod(&[DataValue::from(i64::MIN), DataValue::from(-1)]).is_err());
    assert_eq!(
        op_mod(&[DataValue::from(i64::MIN), DataValue::from(2)]).unwrap(),
        DataValue::from(0)
    );
}

#[test]
fn test_boolean() {
    // `and`/`or` are language forms (`Expr::Lazy`), not ops; their
    // semantics — including short-circuit — are pinned in
    // `data/tests/exprs.rs`. Only negation remains an op.
    assert_eq!(
        op_negate(&[DataValue::from(false)]).unwrap(),
        DataValue::from(true)
    );
}

#[test]
fn test_bits() {
    assert_eq!(
        op_bit_and(&[
            DataValue::Bytes([0b111000].into()),
            DataValue::Bytes([0b010101].into())
        ])
        .unwrap(),
        DataValue::Bytes([0b010000].into())
    );
    assert_eq!(
        op_bit_or(&[
            DataValue::Bytes([0b111000].into()),
            DataValue::Bytes([0b010101].into())
        ])
        .unwrap(),
        DataValue::Bytes([0b111101].into())
    );
    assert_eq!(
        op_bit_not(&[DataValue::Bytes([0b00111000].into())]).unwrap(),
        DataValue::Bytes([0b11000111].into())
    );
    assert_eq!(
        op_bit_xor(&[
            DataValue::Bytes([0b111000].into()),
            DataValue::Bytes([0b010101].into())
        ])
        .unwrap(),
        DataValue::Bytes([0b101101].into())
    );
}

#[test]
fn test_pack_bits() {
    assert_eq!(
        op_pack_bits(&[DataValue::List(vec![DataValue::from(true)])]).unwrap(),
        DataValue::Bytes([0b10000000].into())
    )
}

#[test]
fn test_unpack_bits() {
    assert_eq!(
        op_unpack_bits(&[DataValue::Bytes([0b10101010].into())]).unwrap(),
        DataValue::List(
            [true, false, true, false, true, false, true, false]
                .into_iter()
                .map(DataValue::Bool)
                .collect()
        )
    )
}
