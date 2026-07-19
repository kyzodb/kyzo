/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0): the original `expression_eval` tests ran KyzoScript through a
 * `DbInstance` and are deferred to the parse-tier port; the conditional
 * semantics they exercised are covered here directly on `Expr`.
 * New laws are tested: nondeterministic applications never constant-fold,
 * and deserialized expressions re-prove their arity.
 */

use crate::data::expr::{Expr, LazyOp, Op, apply_op};
use crate::data::functions::{OP_ADD, OP_EQ, OP_NOW, OP_RAND_FLOAT};
use kyzo_model::SourceSpan;
use kyzo_model::value::{DataValue, Vector};

fn cnst(val: impl Into<DataValue>) -> Expr {
    Expr::Const {
        val: val.into(),
        span: Default::default(),
    }
}

// Replaces the upstream `expression_eval` (which needed the parser): a
// conditional falling through to its catch-all yields null, and a true
// clause yields its branch — through `Expr::eval`, the one expression
// authority.
//
// The parser guarantees every `Cond` ends with a catch-all `(true, …)`
// clause (`if`/`cond` append one), so conditionals always net a value;
// both cases here are built the way the parser builds them.
#[test]
fn conditional_eval() {
    // `if(false, 1)` — parser shape: [(false, 1), (true, null)].
    let falls_through = Expr::Cond {
        clauses: vec![(cnst(false), cnst(1)), (cnst(true), cnst(DataValue::Null))],
        span: Default::default(),
    };
    assert_eq!(falls_through.eval(vec![]).unwrap(), DataValue::Null);

    // `if(false, 1, 2)` — parser shape: [(false, 1), (true, 2)].
    let second_true = Expr::Cond {
        clauses: vec![(cnst(false), cnst(1)), (cnst(true), cnst(2))],
        span: Default::default(),
    };
    assert_eq!(second_true.eval(vec![]).unwrap(), DataValue::from(2));
}

// A `Cond` with no catch-all clause is not constructible from source (the
// parser always appends one). If one is hand-built and every clause is
// false, the tree evaluator yields null.
#[test]
fn conditional_without_catch_all() {
    let all_false = Expr::Cond {
        clauses: vec![(cnst(false), cnst(1))],
        span: Default::default(),
    };
    assert_eq!(all_false.eval(vec![]).unwrap(), DataValue::Null);
}

// ─── The op-application NaN checkpoint: structural absorption of any NaN
// result the domain layer missed ─────────────────────────────────────────
//
// A synthetic op stands in for ANY op — present or future — whose own
// `no_nan`/`no_nan_vec_f32`/`no_nan_vec_f64` guard is missing or has a gap:
// its `inner` hands back a bare, unrefused `NaN` unconditionally. The
// checkpoint (`apply_op`, the single site `Expr::eval` routes every
// application through) must refuse it exactly like a real op's own domain
// guard would — independent of the synthetic op ever having a guard of its
// own. This is what proves the class is unrepresentable at the BOUNDARY,
// not merely guarded case by case.

const POISONED_SCALAR: Op = Op::define(
    "OP_POISONED_SCALAR_TEST_ONLY",
    0,
    true,
    true,
    |_| Ok(DataValue::from(f64::NAN)),
);

const POISONED_VECTOR: Op = Op::define(
    "OP_POISONED_VECTOR_TEST_ONLY",
    0,
    true,
    true,
    |_| Ok(DataValue::Vector(Vector::try_new(vec![1.0f64, f64::NAN]).unwrap())),
);

fn assert_domain_refusal(res: miette::Result<DataValue>) {
    let err = res.expect_err("a NaN op result must be a typed Err, not Ok(NaN)");
    assert_eq!(
        err.code().map(|c| c.to_string()),
        Some("eval::domain_error".to_string()),
        "expected the domain_error diagnostic code, got: {err:?}"
    );
}

#[test]
fn apply_op_checkpoint_refuses_scalar_nan_independent_of_the_op() {
    assert_domain_refusal(apply_op(&POISONED_SCALAR, &[]));
}

#[test]
fn apply_op_checkpoint_refuses_vector_nan_lane_independent_of_the_op() {
    assert_domain_refusal(apply_op(&POISONED_VECTOR, &[]));
}

// The checkpoint's refusal still surfaces (as a typed `Err`, wrapped in
// `EvalRaisedError` — see `math_domain_error_surfaces_through_query` in
// `tests/adversarial_robustness.rs` for the query-surface view) through
// the tree walker.
#[test]
fn poisoned_op_is_refused_through_eval() {
    let e = Expr::Apply {
        op: &POISONED_SCALAR,
        args: Box::new([]),
        span: Default::default(),
    };
    assert!(e.eval(&[] as &[DataValue]).is_err());
}

#[test]
fn deterministic_applications_fold() {
    let mut expr = Expr::Apply {
        op: &OP_ADD,
        args: [cnst(1), cnst(2)].into(),
        span: Default::default(),
    };
    expr.partial_eval().unwrap();
    assert_eq!(expr.get_const(), Some(&DataValue::from(3)));
}

#[test]
fn nondeterministic_applications_do_not_fold() {
    // `rand_float()` over constants must stay an application: folding it
    // would freeze one number into every row of a query.
    let mut expr = Expr::Apply {
        op: &OP_RAND_FLOAT,
        args: [].into(),
        span: Default::default(),
    };
    expr.partial_eval().unwrap();
    assert!(matches!(expr, Expr::Apply { .. }));

    // The clock is nondeterministic too: `now()` evaluates per row.
    let mut expr = Expr::Apply {
        op: &OP_NOW,
        args: [].into(),
        span: Default::default(),
    };
    expr.partial_eval().unwrap();
    assert!(matches!(expr, Expr::Apply { .. }));
}

#[test]
fn deserialized_expr_arity_is_rejected() {
    // `eq` requires exactly two arguments; a serialized application with one
    // must be rejected at the serde boundary, before any op body can run.
    let bad = Expr::Apply {
        op: &OP_EQ,
        args: [cnst(1)].into(),
        span: Default::default(),
    };
    let serialized = serde_json::to_string(&bad).unwrap();
    assert!(serde_json::from_str::<Expr>(&serialized).is_err());

    // Positive control: a correct arity round-trips.
    let good = Expr::Apply {
        op: &OP_EQ,
        args: [cnst(1), cnst(2)].into(),
        span: Default::default(),
    };
    let serialized = serde_json::to_string(&good).unwrap();
    assert_eq!(serde_json::from_str::<Expr>(&serialized).unwrap(), good);
}

// ─── The short-circuit law (Expr::Lazy) ─────────────────────────────────
//
// `&&`, `||`, and `~` are language forms: arguments evaluate left to
// right and evaluation STOPS at the deciding argument. A deciding earlier
// argument means later arguments are never touched — their errors never
// fire.

/// An expression that compiles fine and errors at RUNTIME (adding a
/// number to a string is a typed evaluation error).
fn erroring() -> Expr {
    Expr::Apply {
        op: &OP_ADD,
        args: Box::new([cnst(1), cnst("boom")]),
        span: Default::default(),
    }
}

fn lazy(op: LazyOp, args: Vec<Expr>) -> Expr {
    Expr::Lazy {
        op,
        args: args.into(),
        span: Default::default(),
    }
}

#[test]
fn and_short_circuits_past_errors() {
    let e = lazy(LazyOp::And, vec![cnst(false), erroring()]);
    assert_eq!(e.eval(&[] as &[DataValue]).unwrap(), DataValue::from(false));

    // The dual: a non-deciding prefix reaches the error.
    let e = lazy(LazyOp::And, vec![cnst(true), erroring()]);
    assert!(e.eval(&[] as &[DataValue]).is_err());

    // All-true nets true; empty is the identity.
    let e = lazy(LazyOp::And, vec![cnst(true), cnst(true)]);
    assert_eq!(e.eval(&[] as &[DataValue]).unwrap(), DataValue::from(true));
    let e = lazy(LazyOp::And, vec![]);
    assert_eq!(e.eval(&[] as &[DataValue]).unwrap(), DataValue::from(true));
}

#[test]
fn or_short_circuits_past_errors() {
    let e = lazy(LazyOp::Or, vec![cnst(true), erroring()]);
    assert_eq!(e.eval(&[] as &[DataValue]).unwrap(), DataValue::from(true));

    let e = lazy(LazyOp::Or, vec![cnst(false), erroring()]);
    assert!(e.eval(&[] as &[DataValue]).is_err());

    let e = lazy(LazyOp::Or, vec![]);
    assert_eq!(e.eval(&[] as &[DataValue]).unwrap(), DataValue::from(false));
}

#[test]
fn coalesce_short_circuits_past_errors() {
    let e = lazy(LazyOp::Coalesce, vec![cnst(7), erroring()]);
    assert_eq!(e.eval(&[] as &[DataValue]).unwrap(), DataValue::from(7));

    // Null falls through to the next argument.
    let e = lazy(
        LazyOp::Coalesce,
        vec![cnst(DataValue::Null), cnst(42), erroring()],
    );
    assert_eq!(e.eval(&[] as &[DataValue]).unwrap(), DataValue::from(42));

    // All null nets null.
    let e = lazy(
        LazyOp::Coalesce,
        vec![cnst(DataValue::Null), cnst(DataValue::Null)],
    );
    assert_eq!(e.eval(&[] as &[DataValue]).unwrap(), DataValue::Null);
}

#[test]
fn lazy_connectives_type_check_evaluated_arguments() {
    // Non-bool in the evaluated prefix is a typed error…
    let e = lazy(LazyOp::And, vec![cnst(1), cnst(true)]);
    assert!(e.eval(&[] as &[DataValue]).is_err());
    // …but a non-bool PAST the deciding argument is never seen.
    let e = lazy(LazyOp::Or, vec![cnst(true), cnst(1)]);
    assert_eq!(e.eval(&[] as &[DataValue]).unwrap(), DataValue::from(true));
}

#[test]
fn lazy_folding_preserves_short_circuit() {
    // partial_eval folds a closed lazy expression through its own lazy
    // eval: the deciding prefix protects the erroring tail at fold time.
    let mut e = lazy(LazyOp::And, vec![cnst(false), erroring()]);
    e.partial_eval().unwrap();
    assert_eq!(e, cnst(false));
}

#[test]
fn lazy_refusal_spans_point_at_the_argument() {
    // Distinct spans everywhere: a span-blind fixture cannot see a
    // divergence.
    let bad_arg = Expr::Const {
        val: DataValue::from(1),
        span: SourceSpan(30, 4),
    };
    let e = Expr::Lazy {
        op: LazyOp::And,
        args: Box::new([
            Expr::Const {
                val: DataValue::from(true),
                span: SourceSpan(10, 4),
            },
            bad_arg,
        ]),
        span: SourceSpan(10, 100),
    };
    let tree_err = e.eval(&[] as &[DataValue]).unwrap_err();
    // Points at the offending ARGUMENT's span.
    let labels: Vec<(usize, usize)> = tree_err
        .labels()
        .into_iter()
        .flatten()
        .map(|l| (l.offset(), l.len()))
        .collect();
    assert_eq!(labels, vec![(30, 4)], "tree points at the argument");
}
