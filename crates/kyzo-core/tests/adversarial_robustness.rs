/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Story #88: the adversarial-torture battery, locked in permanently. The
//! engine law under test: **no query text may panic the process; every
//! failure is a typed refusal.** Every case here was hand-picked to try to
//! break that law — integer overflow, malformed literals, type confusion,
//! unbalanced/deeply nested syntax, an unstratifiable negation cycle, the
//! reserved temporal sentinel, a self-cross-product, and aggregation over
//! an emptied-out relation — driven only through the public API
//! (`kyzo::Db::run_script`), matching every other file in this directory.
//! A hostile input that panics kills the test process outright, which is
//! itself part of what each `#[test]` here is checking for.

mod common;
use common::*;
use kyzo::{DataValue, Tuple};

/// `i64::MAX + 1` computed inside the query: the arithmetic must overflow
/// into a typed refusal, never wrap, never panic.
#[test]
fn i64_overflow_arithmetic_is_refused() {
    let db = fresh_db();
    let res = db.run_script("?[x] := x = 9223372036854775807 + 1", no_params());
    assert!(
        res.is_err(),
        "i64 overflow must be a typed Err, got {res:?}"
    );
}

/// An integer literal far outside `i64` range must be refused at parse
/// time, never silently truncated/wrapped into some in-range value.
#[test]
fn out_of_range_integer_literal_is_refused() {
    let db = fresh_db();
    let res = db.run_script("?[x] := x = 99999999999999999999999999999999", no_params());
    assert!(
        res.is_err(),
        "an out-of-range integer literal must be a typed Err, got {res:?}"
    );
}

/// Comparing an `Int` against a `String` with `>` is a type mismatch: the
/// engine must refuse it, not silently coerce or panic on the comparison.
#[test]
fn int_vs_string_comparison_is_refused() {
    let db = fresh_db();
    let res = db.run_script("?[x] := x = 1, x > 'z'", no_params());
    assert!(
        res.is_err(),
        "comparing an Int to a String must be a typed Err, got {res:?}"
    );
}

/// Unbalanced parentheses are a syntax error, refused by the parser, never
/// a panic from an unwrapped stack machine.
#[test]
fn unbalanced_parens_is_refused() {
    let db = fresh_db();
    let res = db.run_script("?[x] := x = (((", no_params());
    assert!(
        res.is_err(),
        "unbalanced parens must be a typed Err, got {res:?}"
    );
}

/// Deep-but-legal nesting (20 levels, well under the language's nesting
/// ceiling) must parse and evaluate exactly as if the parens weren't
/// there at all — nesting depth alone is not an error.
#[test]
fn deep_but_legal_nesting_evaluates_correctly() {
    let db = fresh_db();
    let opens = "(".repeat(20);
    let closes = ")".repeat(20);
    let query = format!("?[x] := x = {opens}1{closes}");
    let out = db
        .run_script(&query, no_params())
        .expect("20 levels of nesting is well under the ceiling and must succeed");
    assert_eq!(out.rows.len(), 1);
    assert_eq!(out.rows[0][0].get_int(), Some(1));
}

/// The classic unstratifiable negation cycle (`p` depends on `not q`, `q`
/// depends on `not p`) has no valid stratum order and must be refused,
/// never silently mis-answered. (Also covered from a different angle in
/// `errors_and_refusals.rs`; kept here too so the whole torture battery
/// lives in one place.)
#[test]
fn unstratifiable_negation_cycle_is_refused() {
    let db = fresh_db();
    let query = "p[x] := x = 1, not q[x]; \
                 q[x] := x = 1, not p[x]; \
                 ?[x] := p[x]";
    let res = db.run_script(query, no_params());
    assert!(
        res.is_err(),
        "an unstratifiable negation cycle must be a typed Err, got {res:?}"
    );
}

/// The reserved temporal sentinel: `i64::MAX` is the open-end-of-time
/// marker every interval/validity reads as "still open," so writing a
/// fact AT that exact instant is refused (issue #62's ruling) rather than
/// silently landing a zero-width, unreadable fact.
#[test]
fn extreme_temporal_write_at_i64_max_is_refused() {
    let db = fresh_db();
    let query = format!(
        "?[id, v] <- [[1, 'x']] :create sentinel_probe {{id => v}} @ {}",
        i64::MAX
    );
    let res = db.run_script(&query, no_params());
    assert!(
        res.is_err(),
        "writing at the reserved i64::MAX validity sentinel must be a typed Err, got {res:?}"
    );
}

/// A relation joined against itself four times over a 3-row base produces
/// the full 3^4 = 81-row cross product — no accidental de-duplication, no
/// blow-up crash, just the honest cardinality.
#[test]
fn self_cross_product_is_the_full_cardinality() {
    let db = fresh_db();
    let out = db
        .run_script(
            "a[x] <- [[1],[2],[3]]; ?[w,x,y,z] := a[w],a[x],a[y],a[z]",
            no_params(),
        )
        .expect("a 4-way self cross-product of 3 rows must succeed");
    assert_eq!(out.rows.len(), 81, "3^4 = 81 rows expected");
}

/// Global aggregation (`count`/`min`, no bare head variable) over a
/// relation that has been created but never populated: `count` reports
/// zero and `min` reports `Null` over the empty group — the identity
/// result, not an error and not a panic on an empty running accumulator.
#[test]
fn aggregation_over_empty_relation_is_the_identity_result() {
    let db = fresh_db();
    db.run_script(":create empty_rel {x: Int}", no_params())
        .expect("create empty_rel");

    let out = db
        .run_script("?[count(x), min(x)] := *empty_rel{x}", no_params())
        .expect("aggregation over an empty relation must succeed, not error");
    assert_eq!(
        out.rows.len(),
        1,
        "a global aggregate always yields one row"
    );
    assert_eq!(out.rows[0][0].get_int(), Some(0), "count of nothing is 0");
    assert_eq!(
        out.rows[0][1],
        DataValue::Null,
        "min of nothing is Null, got {:?}",
        out.rows[0][1]
    );
}

// NOTE: unbounded recursion (`f[x] := f[y], x = y+1`) is deliberately
// EXCLUDED from this battery. It currently hangs rather than refusing —
// a real, tracked bug (issue #68: the fixpoint has no default iteration
// budget) — and including it here would hang this entire test suite
// along with it. Fix lands under #68, not here.

/// Division and modulo by zero: NOT pinned to a specific outcome here —
/// that behavior is under active change in a concurrent story. This only
/// proves the call returns a `Result` at all (no panic, no process abort);
/// whichever typed shape (`Ok` or `Err`) the division-by-zero fix lands
/// on, this assertion still holds.
#[test]
fn division_by_zero_never_panics() {
    let db = fresh_db();
    let res = db.run_script("?[x] := x = 1 / 0", no_params());
    let _: miette::Result<kyzo::NamedRows> = res;
}

/// Same non-panic guarantee for modulo by zero.
#[test]
fn modulo_by_zero_never_panics() {
    let db = fresh_db();
    let res = db.run_script("?[x] := x = 1 % 0", no_params());
    let _: miette::Result<kyzo::NamedRows> = res;
}

/// A partial math op fed an out-of-domain value must surface a typed refusal
/// through the *query* surface — not bind a silent `NaN` into the answer
/// set. This locks the whole silent-NaN class (scalar domain ops and the
/// vector-distance ops) at the level a user actually observes, exercising
/// the row evaluator end-to-end, while an in-domain call still answers.
#[test]
fn math_domain_error_surfaces_through_query() {
    let db = fresh_db();
    for script in [
        "?[x] := x = sqrt(-1.0)",
        "?[x] := x = ln(0.0)",
        "?[x] := x = cos_dist([0.0, 0.0], [1.0, 1.0])",
        "?[x] := x = l2_normalize([0.0, 0.0])",
    ] {
        let res = db.run_script(script, no_params());
        assert!(
            res.is_err(),
            "out-of-domain math through a query must be a typed Err, not Ok(NaN): {script} => {res:?}"
        );
    }
    // An in-domain call still answers with the right value.
    let ok = db
        .run_script("?[x] := x = sqrt(4.0)", no_params())
        .expect("in-domain sqrt must answer");
    assert_eq!(ok.rows, vec![Tuple::from_vec(vec![DataValue::from(2.0)])]);
}

/// Story #62's structural checkpoint, not another op-specific guard: an op
/// with NO domain check of its own — `to_float`'s string branch — still
/// cannot hand back `Ok(NaN)` through a query, because the checkpoint sits
/// at the shared op-application site every op routes through, not inside
/// individual ops. `to_float` is a real, previously-unguarded example:
/// its special-cased `'NAN'` literal and (falling through to
/// `f64::from_str`, whose grammar is case-insensitive) `'nan'`/`'NaN'`
/// spellings alike used to answer `Ok(NaN)` silently.
///
/// USER-OBSERVABLE BEHAVIOR CHANGE, pinned here: `to_float('nan')` (and
/// every case spelling of it) now refuses with a typed refusal instead of
/// silently answering with a poisoned float. This is the correct,
/// deliberate outcome per the engine's no-silent-poison ethos — not a
/// regression.
#[test]
fn to_float_nan_is_now_a_typed_refusal() {
    let db = fresh_db();
    for text in ["'NAN'", "'nan'", "'NaN'"] {
        let script = format!("?[x] := x = to_float({text})");
        let res = db.run_script(&script, no_params());
        assert!(
            res.is_err(),
            "to_float({text}) must refuse the NaN it used to answer silently: {res:?}"
        );
    }
    // Non-poison strings are unaffected.
    let ok = db
        .run_script("?[x] := x = to_float('3.5')", no_params())
        .expect("in-domain to_float must still answer");
    assert_eq!(ok.rows, vec![Tuple::from_vec(vec![DataValue::from(3.5)])]);
}

/// The row evaluator's checkpoint is proven above through expressions a
/// compile-time constant fold could evaluate directly; the COLUMNAR
/// evaluator (`query::vm`) needs its own proof, from a `NaN` that only
/// exists at RUNTIME, over a value bound from a stored relation (so
/// `partial_eval` cannot fold it away at compile time and the batch
/// machinery genuinely has to run the op). A stored float large enough
/// that `* 10.0` overflows to `+inf` (legitimate, never refused), then
/// `inf - inf`, is the same unguarded `op_mul`/`op_sub` shape as the
/// scalar case — caught here only by the columnar checkpoint.
#[test]
fn columnar_path_refuses_a_runtime_produced_nan() {
    let db = fresh_db();
    db.run_script("?[x] <- [[1e308]] :create huge {x}", no_params())
        .expect("seed relation");
    let res = db.run_script("?[y] := *huge[x], y = (x * 10.0) - (x * 10.0)", no_params());
    assert!(
        res.is_err(),
        "a runtime inf-minus-inf must be a typed Err through the columnar evaluator: {res:?}"
    );
}
