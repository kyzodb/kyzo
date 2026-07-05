/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0): `test_coalesce` (KyzoScript through a `DbInstance`) is
 * deferred to the parse-tier port and `test_range` calls `op_int_range`
 * directly; the `approx`/`num_traits` dev-dependencies are replaced with a
 * plain closeness check and `std::f64::consts`. New regression tests cover
 * the de-panicked and corrected behaviours: vector multiplication actually
 * multiplies, `vec` rejects non-array JSON, JSON paths reject negative
 * indices, and pre-epoch timestamps parse to negative values.
 */

use std::f64::consts::{E, PI};

use regex::Regex;
use serde_json::json;

use crate::data::functions::*;
use crate::data::relation::{ColType, NullableColType};
use crate::data::value::{DataValue, JsonData, RegexWrapper, ValidityTs, Vector};

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
    DataValue::Vec(Vector::F64(ndarray::Array1::from_vec(xs.to_vec())))
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
fn test_eq_neq() {
    assert_eq!(
        op_eq(&[DataValue::from(1), DataValue::from(1.0)]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_eq(&[DataValue::from(123), DataValue::from(123)]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_neq(&[DataValue::from(1), DataValue::from(1.0)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_neq(&[DataValue::from(123), DataValue::from(123.0)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_eq(&[DataValue::from(123), DataValue::from(123.1)]).unwrap(),
        DataValue::from(false)
    );
}

#[test]
fn test_list() {
    assert_eq!(op_list(&[]).unwrap(), DataValue::List(vec![]));
    assert_eq!(
        op_list(&[DataValue::from(1)]).unwrap(),
        DataValue::List(vec![DataValue::from(1)])
    );
    assert_eq!(
        op_list(&[DataValue::from(1), DataValue::List(vec![])]).unwrap(),
        DataValue::List(vec![DataValue::from(1), DataValue::List(vec![])])
    );
}

#[test]
fn test_is_in() {
    assert_eq!(
        op_is_in(&[
            DataValue::from(1),
            DataValue::List(vec![DataValue::from(1), DataValue::from(2)])
        ])
        .unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_is_in(&[
            DataValue::from(3),
            DataValue::List(vec![DataValue::from(1), DataValue::from(2)])
        ])
        .unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_is_in(&[DataValue::from(3), DataValue::List(vec![])]).unwrap(),
        DataValue::from(false)
    );
}

#[test]
fn test_comparators() {
    assert_eq!(
        op_ge(&[DataValue::from(2), DataValue::from(1)]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_ge(&[DataValue::from(2.), DataValue::from(1)]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_ge(&[DataValue::from(2), DataValue::from(1.)]).unwrap(),
        DataValue::from(true)
    );

    assert_eq!(
        op_ge(&[DataValue::from(1), DataValue::from(1)]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_ge(&[DataValue::from(1), DataValue::from(1.0)]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_ge(&[DataValue::from(1), DataValue::from(2)]).unwrap(),
        DataValue::from(false)
    );
    assert!(op_ge(&[DataValue::Null, DataValue::from(true)]).is_err());
    assert_eq!(
        op_gt(&[DataValue::from(2), DataValue::from(1)]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_gt(&[DataValue::from(2.), DataValue::from(1)]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_gt(&[DataValue::from(2), DataValue::from(1.)]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_gt(&[DataValue::from(1), DataValue::from(1)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_gt(&[DataValue::from(1), DataValue::from(1.0)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_gt(&[DataValue::from(1), DataValue::from(2)]).unwrap(),
        DataValue::from(false)
    );
    assert!(op_gt(&[DataValue::Null, DataValue::from(true)]).is_err());
    assert_eq!(
        op_le(&[DataValue::from(2), DataValue::from(1)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_le(&[DataValue::from(2.), DataValue::from(1)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_le(&[DataValue::from(2), DataValue::from(1.)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_le(&[DataValue::from(1), DataValue::from(1)]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_le(&[DataValue::from(1), DataValue::from(1.0)]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_le(&[DataValue::from(1), DataValue::from(2)]).unwrap(),
        DataValue::from(true)
    );
    assert!(op_le(&[DataValue::Null, DataValue::from(true)]).is_err());
    assert_eq!(
        op_lt(&[DataValue::from(2), DataValue::from(1)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_lt(&[DataValue::from(2.), DataValue::from(1)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_lt(&[DataValue::from(2), DataValue::from(1.)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_lt(&[DataValue::from(1), DataValue::from(1)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_lt(&[DataValue::from(1), DataValue::from(1.0)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_lt(&[DataValue::from(1), DataValue::from(2)]).unwrap(),
        DataValue::from(true)
    );
    assert!(op_lt(&[DataValue::Null, DataValue::from(true)]).is_err());
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
    assert_eq!(
        op_signum(&[DataValue::from(-0.0)]).unwrap(),
        DataValue::from(-1)
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

#[test]
fn test_concat() {
    assert_eq!(
        op_concat(&[DataValue::Str("abc".into()), DataValue::Str("def".into())]).unwrap(),
        DataValue::Str("abcdef".into())
    );

    assert_eq!(
        op_concat(&[
            DataValue::List(vec![DataValue::from(true), DataValue::from(false)]),
            DataValue::List(vec![DataValue::from(true)])
        ])
        .unwrap(),
        DataValue::List(vec![
            DataValue::from(true),
            DataValue::from(false),
            DataValue::from(true),
        ])
    );
}

#[test]
fn test_str_includes() {
    assert_eq!(
        op_str_includes(&[
            DataValue::Str("abcdef".into()),
            DataValue::Str("bcd".into())
        ])
        .unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_str_includes(&[DataValue::Str("abcdef".into()), DataValue::Str("bd".into())]).unwrap(),
        DataValue::from(false)
    );
}

#[test]
fn test_casings() {
    assert_eq!(
        op_lowercase(&[DataValue::Str("NAÏVE".into())]).unwrap(),
        DataValue::Str("naïve".into())
    );
    assert_eq!(
        op_uppercase(&[DataValue::Str("naïve".into())]).unwrap(),
        DataValue::Str("NAÏVE".into())
    );
}

#[test]
fn test_trim() {
    assert_eq!(
        op_trim(&[DataValue::Str(" a ".into())]).unwrap(),
        DataValue::Str("a".into())
    );
    assert_eq!(
        op_trim_start(&[DataValue::Str(" a ".into())]).unwrap(),
        DataValue::Str("a ".into())
    );
    assert_eq!(
        op_trim_end(&[DataValue::Str(" a ".into())]).unwrap(),
        DataValue::Str(" a".into())
    );
}

#[test]
fn test_starts_ends_with() {
    assert_eq!(
        op_starts_with(&[
            DataValue::Str("abcdef".into()),
            DataValue::Str("abc".into())
        ])
        .unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_starts_with(&[DataValue::Str("abcdef".into()), DataValue::Str("bc".into())]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_ends_with(&[
            DataValue::Str("abcdef".into()),
            DataValue::Str("def".into())
        ])
        .unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_ends_with(&[DataValue::Str("abcdef".into()), DataValue::Str("bc".into())]).unwrap(),
        DataValue::from(false)
    );
}

#[test]
fn test_regex() {
    assert_eq!(
        op_regex_matches(&[
            DataValue::Str("abcdef".into()),
            DataValue::Regex(RegexWrapper(Regex::new("c.e").unwrap()))
        ])
        .unwrap(),
        DataValue::from(true)
    );

    assert_eq!(
        op_regex_matches(&[
            DataValue::Str("abcdef".into()),
            DataValue::Regex(RegexWrapper(Regex::new("c.ef$").unwrap()))
        ])
        .unwrap(),
        DataValue::from(true)
    );

    assert_eq!(
        op_regex_matches(&[
            DataValue::Str("abcdef".into()),
            DataValue::Regex(RegexWrapper(Regex::new("c.e$").unwrap()))
        ])
        .unwrap(),
        DataValue::from(false)
    );

    assert_eq!(
        op_regex_replace(&[
            DataValue::Str("abcdef".into()),
            DataValue::Regex(RegexWrapper(Regex::new("[be]").unwrap())),
            DataValue::Str("x".into())
        ])
        .unwrap(),
        DataValue::Str("axcdef".into())
    );

    assert_eq!(
        op_regex_replace_all(&[
            DataValue::Str("abcdef".into()),
            DataValue::Regex(RegexWrapper(Regex::new("[be]").unwrap())),
            DataValue::Str("x".into())
        ])
        .unwrap(),
        DataValue::Str("axcdxf".into())
    );
    assert_eq!(
        op_regex_extract(&[
            DataValue::Str("abCDefGH".into()),
            DataValue::Regex(RegexWrapper(Regex::new("[xayef]|(GH)").unwrap()))
        ])
        .unwrap(),
        DataValue::List(vec![
            DataValue::Str("a".into()),
            DataValue::Str("e".into()),
            DataValue::Str("f".into()),
            DataValue::Str("GH".into()),
        ])
    );
    assert_eq!(
        op_regex_extract_first(&[
            DataValue::Str("abCDefGH".into()),
            DataValue::Regex(RegexWrapper(Regex::new("[xayef]|(GH)").unwrap()))
        ])
        .unwrap(),
        DataValue::Str("a".into()),
    );
    assert_eq!(
        op_regex_extract(&[
            DataValue::Str("abCDefGH".into()),
            DataValue::Regex(RegexWrapper(Regex::new("xyz").unwrap()))
        ])
        .unwrap(),
        DataValue::List(vec![])
    );

    assert_eq!(
        op_regex_extract_first(&[
            DataValue::Str("abCDefGH".into()),
            DataValue::Regex(RegexWrapper(Regex::new("xyz").unwrap()))
        ])
        .unwrap(),
        DataValue::Null
    );
}

#[test]
fn test_predicates() {
    assert_eq!(
        op_is_null(&[DataValue::Null]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_is_null(&[DataValue::Bot]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_is_int(&[DataValue::from(1)]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_is_int(&[DataValue::from(1.0)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_is_float(&[DataValue::from(1)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_is_float(&[DataValue::from(1.0)]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_is_num(&[DataValue::from(1)]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_is_num(&[DataValue::from(1.0)]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_is_num(&[DataValue::Null]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_is_bytes(&[DataValue::Bytes([0b1].into())]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_is_bytes(&[DataValue::Null]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_is_list(&[DataValue::List(vec![])]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_is_list(&[DataValue::Null]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_is_string(&[DataValue::Str("".into())]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_is_string(&[DataValue::Null]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_is_finite(&[DataValue::from(1.0)]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_is_finite(&[DataValue::from(f64::INFINITY)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_is_finite(&[DataValue::from(f64::NAN)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_is_infinite(&[DataValue::from(1.0)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_is_infinite(&[DataValue::from(f64::INFINITY)]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_is_infinite(&[DataValue::from(f64::NEG_INFINITY)]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_is_infinite(&[DataValue::from(f64::NAN)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_is_nan(&[DataValue::from(1.0)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_is_nan(&[DataValue::from(f64::INFINITY)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_is_nan(&[DataValue::from(f64::NEG_INFINITY)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_is_nan(&[DataValue::from(f64::NAN)]).unwrap(),
        DataValue::from(true)
    );
}

#[test]
fn test_prepend_append() {
    assert_eq!(
        op_prepend(&[
            DataValue::List(vec![DataValue::from(1), DataValue::from(2)]),
            DataValue::Null,
        ])
        .unwrap(),
        DataValue::List(vec![
            DataValue::Null,
            DataValue::from(1),
            DataValue::from(2),
        ]),
    );
    assert_eq!(
        op_append(&[
            DataValue::List(vec![DataValue::from(1), DataValue::from(2)]),
            DataValue::Null,
        ])
        .unwrap(),
        DataValue::List(vec![
            DataValue::from(1),
            DataValue::from(2),
            DataValue::Null,
        ]),
    );
}

#[test]
fn test_length() {
    assert_eq!(
        op_length(&[DataValue::Str("abc".into())]).unwrap(),
        DataValue::from(3)
    );
    assert_eq!(
        op_length(&[DataValue::List(vec![])]).unwrap(),
        DataValue::from(0)
    );
    assert_eq!(
        op_length(&[DataValue::Bytes([].into())]).unwrap(),
        DataValue::from(0)
    );
}

#[test]
fn test_unicode_normalize() {
    assert_eq!(
        op_unicode_normalize(&[DataValue::Str("abc".into()), DataValue::Str("nfc".into())])
            .unwrap(),
        DataValue::Str("abc".into())
    )
}

#[test]
fn test_sort_reverse() {
    assert_eq!(
        op_sorted(&[DataValue::List(vec![
            DataValue::from(2.0),
            DataValue::from(1),
            DataValue::from(2),
            DataValue::Null,
        ])])
        .unwrap(),
        DataValue::List(vec![
            DataValue::Null,
            DataValue::from(1),
            DataValue::from(2),
            DataValue::from(2.0),
        ])
    );
    assert_eq!(
        op_reverse(&[DataValue::List(vec![
            DataValue::from(2.0),
            DataValue::from(1),
            DataValue::from(2),
            DataValue::Null,
        ])])
        .unwrap(),
        DataValue::List(vec![
            DataValue::Null,
            DataValue::from(2),
            DataValue::from(1),
            DataValue::from(2.0),
        ])
    )
}

#[test]
fn test_haversine() {
    let d = op_haversine_deg_input(&[
        DataValue::from(0),
        DataValue::from(0),
        DataValue::from(0),
        DataValue::from(180),
    ])
    .unwrap()
    .get_float()
    .unwrap();
    assert!(close(d, PI));

    let d = op_haversine_deg_input(&[
        DataValue::from(90),
        DataValue::from(0),
        DataValue::from(0),
        DataValue::from(123),
    ])
    .unwrap()
    .get_float()
    .unwrap();
    assert!(close(d, PI / 2.));

    let d = op_haversine(&[
        DataValue::from(0),
        DataValue::from(0),
        DataValue::from(0),
        DataValue::from(PI),
    ])
    .unwrap()
    .get_float()
    .unwrap();
    assert!(close(d, PI));
}

#[test]
fn test_deg_rad() {
    assert_eq!(
        op_deg_to_rad(&[DataValue::from(180)]).unwrap(),
        DataValue::from(PI)
    );
    assert_eq!(
        op_rad_to_deg(&[DataValue::from(PI)]).unwrap(),
        DataValue::from(180.0)
    );
}

#[test]
fn test_first_last() {
    assert_eq!(
        op_first(&[DataValue::List(vec![])]).unwrap(),
        DataValue::Null,
    );
    assert_eq!(
        op_last(&[DataValue::List(vec![])]).unwrap(),
        DataValue::Null,
    );
    assert_eq!(
        op_first(&[DataValue::List(vec![
            DataValue::from(1),
            DataValue::from(2),
        ])])
        .unwrap(),
        DataValue::from(1),
    );
    assert_eq!(
        op_last(&[DataValue::List(vec![
            DataValue::from(1),
            DataValue::from(2),
        ])])
        .unwrap(),
        DataValue::from(2),
    );
}

#[test]
fn test_chunks() {
    assert_eq!(
        op_chunks(&[
            DataValue::List(vec![
                DataValue::from(1),
                DataValue::from(2),
                DataValue::from(3),
                DataValue::from(4),
                DataValue::from(5),
            ]),
            DataValue::from(2),
        ])
        .unwrap(),
        DataValue::List(vec![
            DataValue::List(vec![DataValue::from(1), DataValue::from(2)]),
            DataValue::List(vec![DataValue::from(3), DataValue::from(4)]),
            DataValue::List(vec![DataValue::from(5)]),
        ])
    );
    assert_eq!(
        op_chunks_exact(&[
            DataValue::List(vec![
                DataValue::from(1),
                DataValue::from(2),
                DataValue::from(3),
                DataValue::from(4),
                DataValue::from(5),
            ]),
            DataValue::from(2),
        ])
        .unwrap(),
        DataValue::List(vec![
            DataValue::List(vec![DataValue::from(1), DataValue::from(2)]),
            DataValue::List(vec![DataValue::from(3), DataValue::from(4)]),
        ])
    );
    assert_eq!(
        op_windows(&[
            DataValue::List(vec![
                DataValue::from(1),
                DataValue::from(2),
                DataValue::from(3),
                DataValue::from(4),
                DataValue::from(5),
            ]),
            DataValue::from(3),
        ])
        .unwrap(),
        DataValue::List(vec![
            DataValue::List(vec![
                DataValue::from(1),
                DataValue::from(2),
                DataValue::from(3),
            ]),
            DataValue::List(vec![
                DataValue::from(2),
                DataValue::from(3),
                DataValue::from(4),
            ]),
            DataValue::List(vec![
                DataValue::from(3),
                DataValue::from(4),
                DataValue::from(5),
            ]),
        ])
    )
}

#[test]
fn test_get() {
    assert!(op_get(&[DataValue::List(vec![]), DataValue::from(0)]).is_err());
    assert_eq!(
        op_get(&[
            DataValue::List(vec![
                DataValue::from(1),
                DataValue::from(2),
                DataValue::from(3),
            ]),
            DataValue::from(1)
        ])
        .unwrap(),
        DataValue::from(2)
    );
    assert_eq!(
        op_maybe_get(&[DataValue::List(vec![]), DataValue::from(0)]).unwrap(),
        DataValue::Null
    );
    assert_eq!(
        op_maybe_get(&[
            DataValue::List(vec![
                DataValue::from(1),
                DataValue::from(2),
                DataValue::from(3),
            ]),
            DataValue::from(1)
        ])
        .unwrap(),
        DataValue::from(2)
    );
}

#[test]
fn test_slice() {
    assert!(
        op_slice(&[
            DataValue::List(vec![
                DataValue::from(1),
                DataValue::from(2),
                DataValue::from(3),
            ]),
            DataValue::from(1),
            DataValue::from(4)
        ])
        .is_err()
    );

    assert!(
        op_slice(&[
            DataValue::List(vec![
                DataValue::from(1),
                DataValue::from(2),
                DataValue::from(3),
            ]),
            DataValue::from(1),
            DataValue::from(3)
        ])
        .is_ok()
    );

    assert_eq!(
        op_slice(&[
            DataValue::List(vec![
                DataValue::from(1),
                DataValue::from(2),
                DataValue::from(3),
            ]),
            DataValue::from(1),
            DataValue::from(-1)
        ])
        .unwrap(),
        DataValue::List(vec![DataValue::from(2)])
    );
}

#[test]
fn test_chars() {
    assert_eq!(
        op_from_substrings(&[op_chars(&[DataValue::Str("abc".into())]).unwrap()]).unwrap(),
        DataValue::Str("abc".into())
    )
}

#[test]
fn test_encode_decode() {
    assert_eq!(
        op_decode_base64(&[op_encode_base64(&[DataValue::Bytes([1, 2, 3].into())]).unwrap()])
            .unwrap(),
        DataValue::Bytes([1, 2, 3].into())
    )
}

#[test]
fn test_to_string() {
    assert_eq!(
        op_to_string(&[DataValue::from(false)]).unwrap(),
        DataValue::Str("false".into())
    );
}

#[test]
fn test_to_unity() {
    assert_eq!(op_to_unity(&[DataValue::Null]).unwrap(), DataValue::from(0));
    assert_eq!(
        op_to_unity(&[DataValue::from(false)]).unwrap(),
        DataValue::from(0)
    );
    assert_eq!(
        op_to_unity(&[DataValue::from(true)]).unwrap(),
        DataValue::from(1)
    );
    assert_eq!(
        op_to_unity(&[DataValue::from(10)]).unwrap(),
        DataValue::from(1)
    );
    assert_eq!(
        op_to_unity(&[DataValue::from(1.0)]).unwrap(),
        DataValue::from(1)
    );
    assert_eq!(
        op_to_unity(&[DataValue::from(f64::NAN)]).unwrap(),
        DataValue::from(1)
    );
    assert_eq!(
        op_to_unity(&[DataValue::Str("0".into())]).unwrap(),
        DataValue::from(1)
    );
    assert_eq!(
        op_to_unity(&[DataValue::Str("".into())]).unwrap(),
        DataValue::from(0)
    );
    assert_eq!(
        op_to_unity(&[DataValue::List(vec![])]).unwrap(),
        DataValue::from(0)
    );
    assert_eq!(
        op_to_unity(&[DataValue::List(vec![DataValue::Null])]).unwrap(),
        DataValue::from(1)
    );
}

#[test]
fn test_to_float() {
    assert_eq!(
        op_to_float(&[DataValue::Null]).unwrap(),
        DataValue::from(0.0)
    );
    assert_eq!(
        op_to_float(&[DataValue::from(false)]).unwrap(),
        DataValue::from(0.0)
    );
    assert_eq!(
        op_to_float(&[DataValue::from(true)]).unwrap(),
        DataValue::from(1.0)
    );
    assert_eq!(
        op_to_float(&[DataValue::from(1)]).unwrap(),
        DataValue::from(1.0)
    );
    assert_eq!(
        op_to_float(&[DataValue::from(1.0)]).unwrap(),
        DataValue::from(1.0)
    );
    assert!(
        op_to_float(&[DataValue::Str("NAN".into())])
            .unwrap()
            .get_float()
            .unwrap()
            .is_nan()
    );
    assert!(
        op_to_float(&[DataValue::Str("INF".into())])
            .unwrap()
            .get_float()
            .unwrap()
            .is_infinite()
    );
    assert!(
        op_to_float(&[DataValue::Str("NEG_INF".into())])
            .unwrap()
            .get_float()
            .unwrap()
            .is_infinite()
    );
    assert_eq!(
        op_to_float(&[DataValue::Str("3".into())])
            .unwrap()
            .get_float()
            .unwrap(),
        3.
    );
}

#[test]
fn test_rand() {
    let n = op_rand_float(&[]).unwrap().get_float().unwrap();
    assert!(n >= 0.);
    assert!(n <= 1.);
    assert_eq!(
        op_rand_bernoulli(&[DataValue::from(0)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_rand_bernoulli(&[DataValue::from(1)]).unwrap(),
        DataValue::from(true)
    );
    assert!(op_rand_bernoulli(&[DataValue::from(2)]).is_err());
    let n = op_rand_int(&[DataValue::from(100), DataValue::from(200)])
        .unwrap()
        .get_int()
        .unwrap();
    assert!(n >= 100);
    assert!(n <= 200);
    // An empty range is an error, not a panic.
    assert!(op_rand_int(&[DataValue::from(200), DataValue::from(100)]).is_err());
    assert_eq!(
        op_rand_choose(&[DataValue::List(vec![])]).unwrap(),
        DataValue::Null
    );
    assert_eq!(
        op_rand_choose(&[DataValue::List(vec![DataValue::from(123)])]).unwrap(),
        DataValue::from(123)
    );
}

#[test]
fn test_set_ops() {
    assert_eq!(
        op_union(&[
            DataValue::List([1, 2, 3].into_iter().map(DataValue::from).collect()),
            DataValue::List([2, 3, 4].into_iter().map(DataValue::from).collect()),
            DataValue::List([3, 4, 5].into_iter().map(DataValue::from).collect())
        ])
        .unwrap(),
        DataValue::List([1, 2, 3, 4, 5].into_iter().map(DataValue::from).collect())
    );
    assert_eq!(
        op_intersection(&[
            DataValue::List(
                [1, 2, 3, 4, 5, 6]
                    .into_iter()
                    .map(DataValue::from)
                    .collect(),
            ),
            DataValue::List([2, 3, 4].into_iter().map(DataValue::from).collect()),
            DataValue::List([3, 4, 5].into_iter().map(DataValue::from).collect())
        ])
        .unwrap(),
        DataValue::List([3, 4].into_iter().map(DataValue::from).collect())
    );
    assert_eq!(
        op_difference(&[
            DataValue::List(
                [1, 2, 3, 4, 5, 6]
                    .into_iter()
                    .map(DataValue::from)
                    .collect(),
            ),
            DataValue::List([2, 3, 4].into_iter().map(DataValue::from).collect()),
            DataValue::List([3, 4, 5].into_iter().map(DataValue::from).collect())
        ])
        .unwrap(),
        DataValue::List([1, 6].into_iter().map(DataValue::from).collect())
    );
}

#[test]
fn test_uuid() {
    let v1 = op_rand_uuid_v1(&[]).unwrap();
    let v4 = op_rand_uuid_v4(&[]).unwrap();
    assert!(op_is_uuid(&[v4]).unwrap().get_bool().unwrap());
    assert!(op_uuid_timestamp(&[v1]).unwrap().get_float().is_some());
    assert!(op_to_uuid(&[DataValue::from("")]).is_err());
    assert!(op_to_uuid(&[DataValue::from("f3b4958c-52a1-11e7-802a-010203040506")]).is_ok());
}

#[test]
fn test_now() {
    let now = op_now(&[]).unwrap();
    assert!(matches!(now, DataValue::Num(_)));
    let s = op_format_timestamp(&[now]).unwrap();
    let _dt = op_parse_timestamp(&[s]).unwrap();
}

// A pre-epoch datetime is a negative count, not a panic: the upstream
// original unwrapped `duration_since(UNIX_EPOCH)` and aborted the process
// on any user-supplied datetime before 1970.
#[test]
fn test_pre_epoch_timestamps() {
    let secs = op_parse_timestamp(&[DataValue::from("1969-07-20T20:17:00Z")])
        .unwrap()
        .get_float()
        .unwrap();
    assert!(secs < 0.);

    let vld = str2vld("1969-07-20T20:17:00Z").unwrap();
    assert!(vld.0.0 < 0);

    // The schema boundary obeys the same law: coercing a pre-epoch validity
    // string yields negative microseconds.
    let typing = NullableColType {
        coltype: ColType::Validity,
        nullable: false,
    };
    let coerced = typing
        .coerce(
            DataValue::Str("1969-07-20T20:17:00Z".into()),
            ValidityTs(std::cmp::Reverse(0)),
        )
        .unwrap();
    match coerced {
        DataValue::Validity(vld) => assert!(vld.timestamp.0.0 < 0),
        v => panic!("expected a validity, got {v:?}"),
    }
}

#[test]
fn test_to_bool() {
    assert_eq!(
        op_to_bool(&[DataValue::Null]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_to_bool(&[DataValue::from(true)]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_to_bool(&[DataValue::from(false)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_to_bool(&[DataValue::from(0)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_to_bool(&[DataValue::from(0.0)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_to_bool(&[DataValue::from(1)]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_to_bool(&[DataValue::from("")]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_to_bool(&[DataValue::from("a")]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_to_bool(&[DataValue::List(vec![])]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_to_bool(&[DataValue::List(vec![DataValue::from(0)])]).unwrap(),
        DataValue::from(true)
    );
}

// The upstream `test_range` ran `int_range` through a `DbInstance`; the op
// is exercised directly here.
#[test]
fn test_range() {
    assert_eq!(
        op_int_range(&[DataValue::from(1), DataValue::from(5)]).unwrap(),
        DataValue::List([1, 2, 3, 4].into_iter().map(DataValue::from).collect())
    );
    assert_eq!(
        op_int_range(&[DataValue::from(5)]).unwrap(),
        DataValue::List([0, 1, 2, 3, 4].into_iter().map(DataValue::from).collect())
    );
    assert_eq!(
        op_int_range(&[DataValue::from(15), DataValue::from(3), DataValue::from(-2)]).unwrap(),
        DataValue::List(
            [15, 13, 11, 9, 7, 5]
                .into_iter()
                .map(DataValue::from)
                .collect()
        )
    );
}

// Base64 vector payloads must be a whole number of elements: trailing
// bytes are an error, the same exact-length law as schema coercion (the
// upstream original silently ignored them).
#[test]
fn test_vec_rejects_trailing_bytes() {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    // 5 bytes: one whole f32 plus one trailing byte.
    let b64 = STANDARD.encode([0u8, 0, 128, 63, 7]);
    assert!(op_vec(&[DataValue::Str(b64.into())]).is_err());
    // 4 bytes decode cleanly to one f32 (1.0, little-endian).
    let ok = STANDARD.encode([0u8, 0, 128, 63]);
    match op_vec(&[DataValue::Str(ok.into())]).unwrap() {
        DataValue::Vec(v) => assert_eq!(v.len(), 1),
        other => panic!("expected vector, got {other:?}"),
    }
    // The F64 path is equally strict: 9 bytes is one f64 plus trailing.
    let bad64 = STANDARD.encode([0u8; 9]);
    assert!(op_vec(&[DataValue::Str(bad64.into()), DataValue::Str("F64".into())]).is_err());
    let ok64 = STANDARD.encode([0u8; 8]);
    match op_vec(&[DataValue::Str(ok64.into()), DataValue::Str("F64".into())]).unwrap() {
        DataValue::Vec(v) => assert_eq!(v.len(), 1),
        other => panic!("expected vector, got {other:?}"),
    }
}

// `vec` on non-array JSON is an error: the upstream original unwrapped
// `as_array()` and aborted on e.g. `vec(json('{}'))`.
#[test]
fn test_vec_rejects_non_array_json() {
    assert!(op_vec(&[DataValue::Json(JsonData(json!({"a": 1})))]).is_err());
    assert!(op_vec(&[DataValue::Json(JsonData(json!(1)))]).is_err());
    assert!(op_vec(&[DataValue::Json(JsonData(json!("x")))]).is_err());
    // Positive control: a JSON array of numbers converts.
    assert_eq!(
        op_vec(&[DataValue::Json(JsonData(json!([1.0, 2.0])))]).unwrap(),
        DataValue::Vec(Vector::F32(ndarray::Array1::from_vec(vec![1.0f32, 2.0])))
    );
}

// A negative JSON array index is an error: the upstream original cast
// `i64 as usize`, turning `-1` into a huge index (an OOM-scale
// `resize_with` on the write path).
#[test]
fn test_json_path_negative_index_errors() {
    let arr = DataValue::Json(JsonData(json!([1, 2, 3])));
    let neg_path = DataValue::List(vec![DataValue::from(-1)]);
    assert!(op_set_json_path(&[arr.clone(), neg_path.clone(), DataValue::from(9)]).is_err());
    assert!(op_remove_json_path(&[arr.clone(), neg_path.clone()]).is_err());
    assert!(op_get(&[arr.clone(), neg_path]).is_err());
    // Positive control: a valid index works.
    assert_eq!(
        op_get(&[arr, DataValue::List(vec![DataValue::from(1)])]).unwrap(),
        DataValue::from(2)
    );
}

// ===========================================================================
// Date/time behavior-pinning fixtures for the chrono -> jiff migration.
//
// Every expected value below was first captured from the chrono 0.4 /
// chrono-tz 0.10 baseline and then re-verified byte-for-byte against jiff 0.2.
// The `*_agreed` tests are inputs where chrono and jiff produce identical
// results: they pin the preserved public behavior. The `datetime_deltas_vs_chrono`
// test pins the exact inputs where jiff and chrono diverge; each case asserts
// jiff's current output and records chrono's former output in a comment. See
// jiff-migration-report.md for the taxonomy.
// ===========================================================================

// `op_format_timestamp` -> RFC3339 string; helper returns Ok(String)/Err(msg).
fn fmt(args: &[DataValue]) -> Result<String, String> {
    op_format_timestamp(args)
        .map(|v| v.get_str().unwrap().to_string())
        .map_err(|e| e.to_string())
}
fn fmt_n(n: f64) -> Result<String, String> {
    fmt(&[DataValue::from(n)])
}
// `op_parse_timestamp` -> float seconds.
fn parse_secs(s: &str) -> Result<f64, String> {
    op_parse_timestamp(&[DataValue::from(s)])
        .map(|v| v.get_float().unwrap())
        .map_err(|e| e.to_string())
}
// `str2vld` -> signed microseconds.
fn vld_micros(s: &str) -> Result<i64, String> {
    str2vld(s).map(|v| v.0.0).map_err(|e| e.to_string())
}
// Validity coercion path (`data/relation.rs`) -> (micros, is_assert).
fn coerce_vld(s: &str) -> Result<(i64, bool), String> {
    use std::cmp::Reverse;
    let typing = NullableColType {
        coltype: ColType::Validity,
        nullable: false,
    };
    typing
        .coerce(DataValue::Str(s.into()), ValidityTs(Reverse(999)))
        .map(|v| match v {
            DataValue::Validity(vld) => (vld.timestamp.0.0, vld.is_assert.0),
            other => panic!("expected Validity, got {other:?}"),
        })
        .map_err(|e| e.to_string())
}

#[test]
fn format_timestamp_numeric_agreed() {
    // Whole seconds: no fractional digits (chrono SecondsFormat::AutoSi).
    assert_eq!(fmt_n(0.0).unwrap(), "1970-01-01T00:00:00+00:00");
    assert_eq!(fmt_n(1.0).unwrap(), "1970-01-01T00:00:01+00:00");
    assert_eq!(fmt_n(-1.0).unwrap(), "1969-12-31T23:59:59+00:00");
    // UTC renders as `+00:00`, never `Z` (matches chrono's DateTime::to_rfc3339).
    assert_eq!(fmt_n(1600000000.0).unwrap(), "2020-09-13T12:26:40+00:00");
    // Millisecond subsecond -> exactly 3 digits.
    assert_eq!(fmt_n(1.5).unwrap(), "1970-01-01T00:00:01.500+00:00");
    assert_eq!(fmt_n(-1.5).unwrap(), "1969-12-31T23:59:58.500+00:00");
    assert_eq!(
        fmt_n(1600000000.5).unwrap(),
        "2020-09-13T12:26:40.500+00:00"
    );
    assert_eq!(fmt_n(0.123).unwrap(), "1970-01-01T00:00:00.123+00:00");
    assert_eq!(fmt_n(0.001).unwrap(), "1970-01-01T00:00:00.001+00:00");
    // Sub-millisecond input is truncated to whole seconds by the `* 1000` path.
    assert_eq!(fmt_n(0.0005).unwrap(), "1970-01-01T00:00:00+00:00");
    // Fractional beyond milliseconds is dropped (the op keeps only millis).
    assert_eq!(
        fmt_n(1234567890.123456).unwrap(),
        "2009-02-13T23:31:30.123+00:00"
    );
    // Pre-epoch and small positive years (0000..=9999) print 4-digit years.
    assert_eq!(fmt_n(-2208988800.0).unwrap(), "1900-01-01T00:00:00+00:00");
    assert_eq!(fmt_n(-30610224000.0).unwrap(), "1000-01-01T00:00:00+00:00");
    assert_eq!(fmt_n(-46394772000.0).unwrap(), "0499-10-22T11:20:00+00:00");
    assert_eq!(fmt_n(-62135596800.0).unwrap(), "0001-01-01T00:00:00+00:00");
    assert_eq!(fmt_n(-62162035200.0).unwrap(), "0000-03-01T00:00:00+00:00");
}

#[test]
fn format_timestamp_validity_input_agreed() {
    use crate::data::value::Validity;
    use std::cmp::Reverse;
    let f = |micros: i64| {
        let vld = DataValue::Validity(Validity {
            timestamp: ValidityTs(Reverse(micros)),
            is_assert: Reverse(true),
        });
        fmt(&[vld]).unwrap()
    };
    // Validity stores microseconds; the op divides by 1000 to milliseconds.
    assert_eq!(f(0), "1970-01-01T00:00:00+00:00");
    assert_eq!(f(1_500_000), "1970-01-01T00:00:01.500+00:00");
    assert_eq!(f(-1_500_000), "1969-12-31T23:59:58.500+00:00");
    assert_eq!(f(1_600_000_000_000_000), "2020-09-13T12:26:40+00:00");
}

#[test]
fn format_timestamp_timezone_agreed() {
    let z = |n: f64, tz: &str| fmt(&[DataValue::from(n), DataValue::from(tz)]);
    // Named zones apply the offset in effect AT that instant (incl. DST).
    assert_eq!(z(0.0, "UTC").unwrap(), "1970-01-01T00:00:00+00:00");
    assert_eq!(
        z(0.0, "Asia/Shanghai").unwrap(),
        "1970-01-01T08:00:00+08:00"
    );
    assert_eq!(
        z(0.0, "America/New_York").unwrap(),
        "1969-12-31T19:00:00-05:00"
    );
    assert_eq!(
        z(0.0, "Europe/London").unwrap(),
        "1970-01-01T01:00:00+01:00"
    );
    assert_eq!(z(0.0, "Asia/Kolkata").unwrap(), "1970-01-01T05:30:00+05:30");
    // September 2020: New York is on EDT (-04:00), not EST.
    assert_eq!(
        z(1600000000.0, "America/New_York").unwrap(),
        "2020-09-13T08:26:40-04:00"
    );
    assert_eq!(
        z(1600000000.5, "Asia/Kolkata").unwrap(),
        "2020-09-13T17:56:40.500+05:30"
    );
    // Unknown / empty zone is rejected with the same message chrono produced.
    assert_eq!(
        z(0.0, "bogus/zone").unwrap_err(),
        "bad timezone specification: bogus/zone"
    );
    assert_eq!(z(0.0, "").unwrap_err(), "bad timezone specification: ");
}

#[test]
fn parse_timestamp_agreed() {
    assert_eq!(parse_secs("1970-01-01T00:00:00Z").unwrap(), 0.0);
    assert_eq!(parse_secs("2020-01-01T00:00:00Z").unwrap(), 1577836800.0);
    assert_eq!(parse_secs("2020-01-01T00:00:00.5Z").unwrap(), 1577836800.5);
    assert_eq!(
        parse_secs("2020-01-01T00:00:00+08:00").unwrap(),
        1577808000.0
    );
    assert_eq!(
        parse_secs("2020-01-01T00:00:00-05:00").unwrap(),
        1577854800.0
    );
    assert_eq!(
        parse_secs("2020-01-01T00:00:00-00:00").unwrap(),
        1577836800.0
    );
    // Pre-epoch parses to a negative count, not a panic (pinned regression).
    assert_eq!(parse_secs("1969-07-20T20:17:00Z").unwrap(), -14182980.0);
    assert_eq!(parse_secs("1969-12-31T23:59:59.5Z").unwrap(), -0.5);
    assert_eq!(parse_secs("1900-01-01T00:00:00Z").unwrap(), -2208988800.0);
    assert_eq!(parse_secs("0000-01-01T00:00:00Z").unwrap(), -62167219200.0);
    // RFC3339 leniencies both libraries share: space separator, lowercase t/z.
    assert_eq!(parse_secs("2020-01-01 00:00:00Z").unwrap(), 1577836800.0);
    assert_eq!(parse_secs("2020-01-01t00:00:00z").unwrap(), 1577836800.0);
    // Rejections shared with chrono.
    for bad in [
        "garbage",
        "",
        "2020-01-01",
        "2020-01-01T00:00:00",
        "2020-13-01T00:00:00Z",
        "2020-02-30T00:00:00Z",
        "2020-01-01T00:00:00.Z",
    ] {
        assert!(parse_secs(bad).is_err(), "expected {bad:?} to be rejected");
    }
}

/// `op_parse_timestamp` returns lossy f64 seconds. For sub-microsecond pre-epoch
/// instants the two-term sum can land on a non-obvious double; jiff reproduces
/// chrono's exact bit pattern because both floor the whole second and add a
/// non-negative subsecond fraction. Pins that bit-identity.
#[test]
fn parse_timestamp_sub_microsecond_matches_chrono() {
    assert_eq!(
        parse_secs("1969-12-31T23:59:59.123456789Z").unwrap(),
        -0.876543211
    );
    assert_eq!(
        parse_secs("1970-01-01T00:00:00.123456789Z").unwrap(),
        0.123456789
    );
    assert_eq!(
        parse_secs("1970-01-01T00:00:00.0000005Z").unwrap(),
        0.0000005
    );
    // The ULP-noisy pair chrono produced (floored second -1 + 0.9999995):
    assert_eq!(
        parse_secs("1969-12-31T23:59:59.9999995Z").unwrap(),
        -0.0000004999999999588667
    );
    assert_eq!(
        parse_secs("1969-12-31T23:59:59.999999Z").unwrap(),
        -0.0000010000000000287557
    );
}

#[test]
fn str2vld_agreed() {
    assert_eq!(vld_micros("1970-01-01T00:00:00Z").unwrap(), 0);
    assert_eq!(
        vld_micros("2020-01-01T00:00:00Z").unwrap(),
        1577836800000000
    );
    assert_eq!(
        vld_micros("2020-01-01T00:00:00.123456Z").unwrap(),
        1577836800123456
    );
    assert_eq!(
        vld_micros("2020-01-01T00:00:00+08:00").unwrap(),
        1577808000000000
    );
    // Pre-epoch validity is a negative microsecond count.
    assert_eq!(vld_micros("1969-07-20T20:17:00Z").unwrap(), -14182980000000);
    assert_eq!(vld_micros("1969-12-31T23:59:59.5Z").unwrap(), -500000);
    assert_eq!(
        vld_micros("1900-01-01T00:00:00Z").unwrap(),
        -2208988800000000
    );
    assert_eq!(
        vld_micros("0000-01-01T00:00:00Z").unwrap(),
        -62167219200000000
    );
    for bad in ["garbage", "", "2020-01-01", "2020-13-01T00:00:00Z"] {
        assert!(vld_micros(bad).is_err(), "expected {bad:?} to be rejected");
    }
}

/// Microsecond timestamps floor toward negative infinity (the microsecond that
/// *contains* the instant), UNIFORMLY on both sides of the epoch, on both the
/// `str2vld` and validity-coercion paths. chrono floored; jiff's `as_microsecond`
/// truncates toward zero, so this pins the shared `timestamp_to_micros` helper
/// that restores the floor. Sub-microsecond finer than a µs is dropped downward.
#[test]
fn validity_micros_floor_agreed_pre_and_post_epoch() {
    // (input, floored micros) — chrono baseline; jiff matches after the fix.
    let cases: &[(&str, i64)] = &[
        // Post-epoch: floor == truncate, unaffected by the fix.
        ("1970-01-01T00:00:00.123456789Z", 123456),
        ("1970-01-01T00:00:00.0000005Z", 0),
        ("2020-01-01T00:00:00.123456Z", 1577836800123456),
        // Pre-epoch sub-microsecond: floor differs from truncate by 1 µs.
        ("1969-12-31T23:59:59.123456789Z", -876544),
        ("1969-12-31T23:59:59.9999995Z", -1),
        ("1969-12-31T23:59:59.999999Z", -1),
        ("1969-12-31T23:59:59.000001Z", -999999),
    ];
    for &(s, micros) in cases {
        assert_eq!(vld_micros(s).unwrap(), micros, "str2vld({s:?})");
        assert_eq!(coerce_vld(s).unwrap(), (micros, true), "coerce({s:?})");
    }
}

#[test]
fn validity_coerce_agreed() {
    assert_eq!(coerce_vld("ASSERT").unwrap(), (999, true));
    assert_eq!(coerce_vld("RETRACT").unwrap(), (999, false));
    assert_eq!(
        coerce_vld("2020-01-01T00:00:00Z").unwrap(),
        (1577836800000000, true)
    );
    assert_eq!(
        coerce_vld("~2020-01-01T00:00:00Z").unwrap(),
        (1577836800000000, false)
    );
    assert_eq!(
        coerce_vld("2020-01-01T00:00:00.123456Z").unwrap(),
        (1577836800123456, true)
    );
    assert_eq!(
        coerce_vld("2020-01-01T00:00:00+08:00").unwrap(),
        (1577808000000000, true)
    );
    // Pre-epoch, asserted and retracted.
    assert_eq!(
        coerce_vld("1969-07-20T20:17:00Z").unwrap(),
        (-14182980000000, true)
    );
    assert_eq!(
        coerce_vld("~1969-07-20T20:17:00Z").unwrap(),
        (-14182980000000, false)
    );
    for bad in ["garbage", "~garbage", ""] {
        assert!(coerce_vld(bad).is_err(), "expected {bad:?} to be rejected");
    }
}

/// Pins every input where jiff diverges from the chrono baseline. Each assert
/// locks jiff's CURRENT behavior; the trailing comment is chrono's old result.
/// If a future jiff upgrade shifts any of these, this test fails loudly rather
/// than letting a semantics change land silently.
#[test]
fn datetime_deltas_vs_chrono() {
    // -- Delta 1: leap-second ":60". chrono folds :60 UP to the next second;
    //    jiff clamps it DOWN to :59. A ":60" timestamp parses 1 second earlier.
    assert_eq!(parse_secs("2016-12-31T23:59:60Z").unwrap(), 1483228799.0); // chrono: 1483228800.0
    assert_eq!(parse_secs("2020-01-01T00:00:60Z").unwrap(), 1577836859.0); // chrono: 1577836860.0
    assert_eq!(
        vld_micros("2016-12-31T23:59:60Z").unwrap(),
        1483228799000000
    ); // chrono: 1483228800000000

    // -- Delta 2: representable range. jiff's Timestamp spans
    //    -009999-01-02T01:59:59Z ..= 9999-12-30T22:00:00.999999999Z (it reserves
    //    ~26h at each end so any instant maps into any tz without overflow).
    //    chrono reached year +-262143. Inputs at/above the RFC3339 max now error.
    assert!(fmt_n(253402300799.0).is_err()); // chrono OK: "9999-12-31T23:59:59+00:00"
    assert!(fmt_n(253402300800.0).is_err()); // chrono OK: "+10000-01-01T00:00:00+00:00" (non-RFC3339)
    assert!(fmt_n(1.5e12).is_err()); // chrono OK: "+49503-02-10T02:40:00+00:00" (non-RFC3339)
    assert!(fmt_n(-4.0e11).is_err()); // chrono OK: "-10706-07-03T08:53:20+00:00"
    assert!(parse_secs("9999-12-31T23:59:59Z").is_err()); // chrono OK: 253402300799.0

    // -- Delta 3: expanded-year FORMAT for in-range NEGATIVE years. jiff emits a
    //    signed 6-digit year; chrono emitted a signed minimal-width year. Only
    //    affects years < 0 (positive years 0000..=9999 are byte-identical).
    assert_eq!(
        fmt_n(-250000000000.0).unwrap(),
        "-005953-10-25T11:33:20+00:00"
    ); // chrono: "-5953-10-25T11:33:20+00:00"

    // -- Delta 4: jiff ACCEPTS parse forms chrono rejected (Temporal/RFC9557).
    //    Each was `bad datetime` under chrono; jiff yields the instant below.
    assert_eq!(parse_secs("+002020-01-01T00:00:00Z").unwrap(), 1577836800.0); // expanded-year sign
    assert_eq!(
        parse_secs("2020-01-01T00:00:00+0000").unwrap(),
        1577836800.0
    ); // offset without colon
    assert_eq!(
        parse_secs("2020-01-01T00:00:00Z[UTC]").unwrap(),
        1577836800.0
    ); // RFC9557 zone annotation
    assert_eq!(parse_secs("2020-01-01T00:00:00,5Z").unwrap(), 1577836800.5); // comma decimal separator
    assert_eq!(
        parse_secs("2020-01-01T00:00:00+05:30:30").unwrap(),
        1577816970.0
    ); // offset with seconds
    assert_eq!(parse_secs("2020-01-01T00:00Z").unwrap(), 1577836800.0); // seconds field omitted
    assert_eq!(
        parse_secs("2020-01-01T00:00:00+24:00").unwrap(),
        1577750400.0
    ); // out-of-range (>23:59) offset
    // The widening reaches the validity paths too.
    assert_eq!(
        vld_micros("2020-01-01T00:00:00,5Z").unwrap(),
        1577836800500000
    ); // chrono: ERR
    assert_eq!(
        coerce_vld("2020-01-01T00:00:00+05:30:30").unwrap(),
        (1577816970000000, true)
    ); // chrono: ERR

    // -- Delta 5: jiff REJECTS > 9 fractional digits; chrono truncated silently.
    assert!(parse_secs("2020-01-01T00:00:00.1234567890123Z").is_err()); // chrono OK: 1577836800.1234567

    // NOTE: the microsecond floor difference (chrono floors, jiff's
    // `as_microsecond` truncates) is NOT listed here: it is fixed in
    // `timestamp_to_micros` and pinned as agreed behavior in
    // `validity_micros_floor_agreed_pre_and_post_epoch`.
}
