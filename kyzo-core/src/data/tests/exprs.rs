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
 * semantics they exercised are covered here directly on `Expr`/`Bytecode`.
 * New laws are tested: nondeterministic applications never constant-fold,
 * and deserialized expressions and bytecode re-prove their arity.
 */

use crate::data::expr::{Bytecode, Expr, Op, apply_op, eval_bytecode};
use crate::data::functions::{OP_ADD, OP_EQ, OP_NOW, OP_RAND_FLOAT};
use crate::data::value::{DataValue, Vector};

fn cnst(val: impl Into<DataValue>) -> Expr {
    Expr::Const {
        val: val.into(),
        span: Default::default(),
    }
}

// Replaces the upstream `expression_eval` (which needed the parser): a
// conditional falling through to its catch-all yields null, and a true
// clause yields its branch — through both the tree evaluator and the
// compiled bytecode, so `expr2bytecode` is exercised where it now lives.
//
// The parser guarantees every `Cond` ends with a catch-all `(true, …)`
// clause (`if`/`cond` append one), so compiled conditionals always net a
// value; both cases here are built the way the parser builds them.
#[test]
fn conditional_eval() {
    let mut stack = vec![];

    // `if(false, 1)` — parser shape: [(false, 1), (true, null)].
    let falls_through = Expr::Cond {
        clauses: vec![(cnst(false), cnst(1)), (cnst(true), cnst(DataValue::Null))],
        span: Default::default(),
    };
    assert_eq!(falls_through.eval(vec![]).unwrap(), DataValue::Null);
    let compiled = falls_through.compile().unwrap();
    assert_eq!(
        eval_bytecode(&compiled, vec![], &mut stack).unwrap(),
        DataValue::Null
    );

    // `if(false, 1, 2)` — parser shape: [(false, 1), (true, 2)].
    let second_true = Expr::Cond {
        clauses: vec![(cnst(false), cnst(1)), (cnst(true), cnst(2))],
        span: Default::default(),
    };
    assert_eq!(second_true.eval(vec![]).unwrap(), DataValue::from(2));
    let compiled = second_true.compile().unwrap();
    assert_eq!(
        eval_bytecode(&compiled, vec![], &mut stack).unwrap(),
        DataValue::from(2)
    );
}

// A `Cond` with no catch-all clause is not constructible from source (the
// parser always appends one). If one is hand-built and every clause is
// false, the tree evaluator yields null, and the compiled program — which
// then nets no value — reports corrupt bytecode as an *error*, never a
// panic. This pins the de-panicked behaviour of the evaluator.
#[test]
fn conditional_without_catch_all() {
    let all_false = Expr::Cond {
        clauses: vec![(cnst(false), cnst(1))],
        span: Default::default(),
    };
    assert_eq!(all_false.eval(vec![]).unwrap(), DataValue::Null);
    let compiled = all_false.compile().unwrap();
    let mut stack = vec![];
    assert!(eval_bytecode(&compiled, vec![], &mut stack).is_err());
}

// ─── The op-application NaN checkpoint: structural absorption of any NaN
// result the domain layer missed ─────────────────────────────────────────
//
// A synthetic op stands in for ANY op — present or future — whose own
// `no_nan`/`no_nan_vec_f32`/`no_nan_vec_f64` guard is missing or has a gap:
// its `inner` hands back a bare, unrefused `NaN` unconditionally. The
// checkpoint (`apply_op`, the single site both `Expr::eval` and
// `eval_bytecode` route every application through) must refuse it exactly
// like a real op's own domain guard would — independent of the synthetic
// op ever having a guard of its own. This is what proves the class is
// unrepresentable at the BOUNDARY, not merely guarded case by case.

const POISONED_SCALAR: Op = Op {
    name: "OP_POISONED_SCALAR_TEST_ONLY",
    min_arity: 0,
    vararg: true,
    deterministic: true,
    inner: |_| Ok(DataValue::from(f64::NAN)),
};

const POISONED_VECTOR: Op = Op {
    name: "OP_POISONED_VECTOR_TEST_ONLY",
    min_arity: 0,
    vararg: true,
    deterministic: true,
    inner: |_| {
        Ok(DataValue::Vec(Vector::F32(ndarray::arr1(&[
            1.0f32,
            f32::NAN,
        ]))))
    },
};

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
// BOTH evaluators the application sites route through: the tree walker
// and the compiled bytecode machine.
#[test]
fn poisoned_op_is_refused_through_both_machines() {
    let e = Expr::Apply {
        op: &POISONED_SCALAR,
        args: Box::new([]),
        span: Default::default(),
    };
    assert!(e.eval(&[] as &[DataValue]).is_err());
    let compiled = e.compile().unwrap();
    let mut stack = vec![];
    assert!(eval_bytecode(&compiled, &[] as &[DataValue], &mut stack).is_err());
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

#[test]
fn deserialized_bytecode_arity_is_rejected() {
    let bad = Bytecode::Apply {
        op: &OP_EQ,
        arity: 1,
        span: Default::default(),
    };
    let serialized = serde_json::to_string(&bad).unwrap();
    assert!(serde_json::from_str::<Bytecode>(&serialized).is_err());

    // Positive control: a correct arity round-trips.
    let good = Bytecode::Apply {
        op: &OP_EQ,
        arity: 2,
        span: Default::default(),
    };
    let serialized = serde_json::to_string(&good).unwrap();
    assert_eq!(serde_json::from_str::<Bytecode>(&serialized).unwrap(), good);
}

// ─── The short-circuit law (Expr::Lazy) ─────────────────────────────────
//
// `&&`, `||`, and `~` are language forms: arguments evaluate left to
// right and evaluation STOPS at the deciding argument. A deciding earlier
// argument means later arguments are never touched — their errors never
// fire. Pinned through BOTH evaluators: any divergence between the tree
// and the bytecode machine is a bug by definition.

/// An expression that compiles fine and errors at RUNTIME (adding a
/// number to a string is a typed evaluation error in both machines).
fn erroring() -> Expr {
    Expr::Apply {
        op: &OP_ADD,
        args: Box::new([cnst(1), cnst("boom")]),
        span: Default::default(),
    }
}

fn lazy(op: crate::data::expr::LazyOp, args: Vec<Expr>) -> Expr {
    Expr::Lazy {
        op,
        args: args.into(),
        span: Default::default(),
    }
}

fn eval_both_ways(e: &Expr) -> (miette::Result<DataValue>, miette::Result<DataValue>) {
    let tree = e.eval(&[] as &[DataValue]);
    let mut prog = vec![];
    let byte = crate::data::expr::expr2bytecode(e, &mut prog)
        .and_then(|()| eval_bytecode(&prog, &[] as &[DataValue], &mut vec![]));
    (tree, byte)
}

#[test]
fn and_short_circuits_past_errors() {
    use crate::data::expr::LazyOp;
    let e = lazy(LazyOp::And, vec![cnst(false), erroring()]);
    let (tree, byte) = eval_both_ways(&e);
    assert_eq!(tree.unwrap(), DataValue::from(false));
    assert_eq!(byte.unwrap(), DataValue::from(false));

    // The dual: a non-deciding prefix reaches the error.
    let e = lazy(LazyOp::And, vec![cnst(true), erroring()]);
    let (tree, byte) = eval_both_ways(&e);
    assert!(tree.is_err());
    assert!(byte.is_err());

    // All-true nets true; empty is the identity.
    let e = lazy(LazyOp::And, vec![cnst(true), cnst(true)]);
    assert_eq!(e.eval(&[] as &[DataValue]).unwrap(), DataValue::from(true));
    let e = lazy(LazyOp::And, vec![]);
    let (tree, byte) = eval_both_ways(&e);
    assert_eq!(tree.unwrap(), DataValue::from(true));
    assert_eq!(byte.unwrap(), DataValue::from(true));
}

#[test]
fn or_short_circuits_past_errors() {
    use crate::data::expr::LazyOp;
    let e = lazy(LazyOp::Or, vec![cnst(true), erroring()]);
    let (tree, byte) = eval_both_ways(&e);
    assert_eq!(tree.unwrap(), DataValue::from(true));
    assert_eq!(byte.unwrap(), DataValue::from(true));

    let e = lazy(LazyOp::Or, vec![cnst(false), erroring()]);
    let (tree, byte) = eval_both_ways(&e);
    assert!(tree.is_err());
    assert!(byte.is_err());

    let e = lazy(LazyOp::Or, vec![]);
    let (tree, byte) = eval_both_ways(&e);
    assert_eq!(tree.unwrap(), DataValue::from(false));
    assert_eq!(byte.unwrap(), DataValue::from(false));
}

#[test]
fn coalesce_short_circuits_past_errors() {
    use crate::data::expr::LazyOp;
    let e = lazy(LazyOp::Coalesce, vec![cnst(7), erroring()]);
    let (tree, byte) = eval_both_ways(&e);
    assert_eq!(tree.unwrap(), DataValue::from(7));
    assert_eq!(byte.unwrap(), DataValue::from(7));

    // Null falls through to the next argument.
    let e = lazy(
        LazyOp::Coalesce,
        vec![cnst(DataValue::Null), cnst(42), erroring()],
    );
    let (tree, byte) = eval_both_ways(&e);
    assert_eq!(tree.unwrap(), DataValue::from(42));
    assert_eq!(byte.unwrap(), DataValue::from(42));

    // All null nets null.
    let e = lazy(
        LazyOp::Coalesce,
        vec![cnst(DataValue::Null), cnst(DataValue::Null)],
    );
    let (tree, byte) = eval_both_ways(&e);
    assert_eq!(tree.unwrap(), DataValue::Null);
    assert_eq!(byte.unwrap(), DataValue::Null);
}

#[test]
fn lazy_connectives_type_check_evaluated_arguments() {
    use crate::data::expr::LazyOp;
    // Non-bool in the evaluated prefix is a typed error in both machines…
    let e = lazy(LazyOp::And, vec![cnst(1), cnst(true)]);
    let (tree, byte) = eval_both_ways(&e);
    assert!(tree.is_err());
    assert!(byte.is_err());
    // …but a non-bool PAST the deciding argument is never seen.
    let e = lazy(LazyOp::Or, vec![cnst(true), cnst(1)]);
    let (tree, byte) = eval_both_ways(&e);
    assert_eq!(tree.unwrap(), DataValue::from(true));
    assert_eq!(byte.unwrap(), DataValue::from(true));
}

#[test]
fn lazy_folding_preserves_short_circuit() {
    use crate::data::expr::LazyOp;
    // partial_eval folds a closed lazy expression through its own lazy
    // eval: the deciding prefix protects the erroring tail at fold time.
    let mut e = lazy(LazyOp::And, vec![cnst(false), erroring()]);
    e.partial_eval().unwrap();
    assert_eq!(e, cnst(false));
}

#[test]
fn lazy_refusal_spans_agree_between_machines() {
    use crate::data::expr::LazyOp;
    use crate::data::span::SourceSpan;
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
    let mut prog = vec![];
    crate::data::expr::expr2bytecode(&e, &mut prog).unwrap();
    let byte_err = eval_bytecode(&prog, &[] as &[DataValue], &mut vec![]).unwrap_err();
    // Same error, same location: the offending ARGUMENT's span.
    assert_eq!(tree_err.to_string(), byte_err.to_string());
    let labels = |e: &miette::Report| -> Vec<(usize, usize)> {
        e.labels()
            .into_iter()
            .flatten()
            .map(|l| (l.offset(), l.len()))
            .collect()
    };
    assert_eq!(
        labels(&tree_err),
        vec![(30, 4)],
        "tree points at the argument"
    );
    assert_eq!(
        labels(&byte_err),
        vec![(30, 4)],
        "bytecode points at the argument"
    );
}
