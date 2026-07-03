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

use crate::data::expr::{Bytecode, Expr, eval_bytecode};
use crate::data::functions::{OP_ADD, OP_EQ, OP_NOW, OP_RAND_FLOAT};
use crate::data::value::DataValue;

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
