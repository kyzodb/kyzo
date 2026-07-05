/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The columnar expression evaluator: one kernel invocation per expression
//! node per BATCH, instead of one tree walk per row.
//!
//! Control flow is SELECTION PARTITIONING, not jumps: the lazy
//! connectives and `Cond` split the live selection and evaluate each
//! sub-expression only over the rows that reach it, which is
//! short-circuit semantics made columnar (a deciding argument's dead rows
//! never touch later arguments, so their errors never fire — the same
//! law `data/expr.rs`'s row evaluator implements). This is the shape
//! DuckDB's and Velox's expression executors settled on, and for the
//! same reason: with partitioning as control, straight-line kernels are
//! all that remains, and a jump-shaped bytecode would add bookkeeping
//! without removing any per-row work.
//!
//! Two laws, both pinned by the differential in this module's tests:
//!
//! - **Observational identity**: for every expression and every batch,
//!   the columnar result equals row-by-row [`Expr::eval`] exactly —
//!   values, presence, and error IDENTITY (the first failing row in row
//!   order; within a row, the first failing subexpression in evaluation
//!   order). Kernels never raise mid-batch; they record `(row, node)`
//!   candidates in an [`ErrorMin`] and the minimum raises at extraction.
//! - **Totality**: every operator evaluates through the generic kernel
//!   (gather the argument rows, call the op function, store), so no
//!   expression needs a row-at-a-time escape hatch. Typed kernels for
//!   hot operators substitute into `apply_op` as measured optimizations,
//!   never as semantic forks.

use miette::Result;

use crate::data::batch::{ColumnBatch, ErrorMin, Selection};
use crate::data::expr::{Decision, Expr};
use crate::data::value::DataValue;

/// A column of values ALIGNED TO A SELECTION: `values[i]` is the result
/// for the `i`-th live row of the selection it was computed under. The
/// alignment is positional, which is what lets `Cond` stitch arm results
/// back together by selection order alone.
struct SelAligned {
    values: Vec<DataValue>,
}

/// Evaluation state threaded through one batch evaluation: the batch, a
/// monotone node counter (evaluation order for error identity), and the
/// running minimum error.
struct BatchEval<'a> {
    batch: &'a ColumnBatch,
    next_node: u32,
    errors: ErrorMin<miette::Report>,
}

impl BatchEval<'_> {
    fn node_id(&mut self) -> u32 {
        let id = self.next_node;
        self.next_node += 1;
        id
    }
}

/// Evaluate `expr` over the live rows of `batch`, columnar. Returns the
/// per-live-row results in selection order, or the exact error row-by-row
/// evaluation would raise first.
pub(crate) fn eval_expr_batched(
    expr: &Expr,
    batch: &ColumnBatch,
    selection: &Selection,
) -> Result<Vec<DataValue>> {
    debug_assert!(
        selection.iter().all(|r| r < batch.height()),
        "selection beyond batch height"
    );
    let mut state = BatchEval {
        batch,
        next_node: 0,
        errors: ErrorMin::default(),
    };
    let out = eval_node(expr, selection, &mut state)?;
    match state.errors.into_error() {
        // An error on a live row: row evaluation would have raised it.
        Some(err) => Err(err),
        None => Ok(out.values),
    }
}

/// Evaluate a filter predicate over the live rows: the refined selection
/// (rows where the predicate is `true`), or a typed error exactly where
/// row evaluation would raise one (including non-boolean predicate
/// values, reported with the predicate's span).
pub(crate) fn eval_pred_batched(
    expr: &Expr,
    batch: &ColumnBatch,
    selection: &Selection,
) -> Result<Selection> {
    use crate::data::expr::PredicateTypeError;
    let values = eval_expr_batched(expr, batch, selection)?;
    let mut keep = Vec::with_capacity(values.len());
    for (v, row) in values.iter().zip(selection.iter()) {
        match v.get_bool() {
            Some(true) => {
                #[allow(clippy::cast_possible_truncation)]
                keep.push(row as u32);
            }
            Some(false) => {}
            None => {
                return Err(PredicateTypeError(expr.span(), v.clone()).into());
            }
        }
    }
    Ok(Selection::from_sorted(keep))
}

fn eval_node(expr: &Expr, sel: &Selection, state: &mut BatchEval<'_>) -> Result<SelAligned> {
    let node = state.node_id();
    match expr {
        Expr::Binding { var, tuple_pos, .. } => match tuple_pos {
            None => {
                use crate::data::expr::UnboundVariableError;
                Err(UnboundVariableError(var.name.to_string(), var.span).into())
            }
            Some(i) => {
                if *i >= state.batch.width() {
                    use crate::data::expr::TupleTooShortError;
                    return Err(TupleTooShortError(
                        var.name.to_string(),
                        *i,
                        state.batch.width(),
                        var.span,
                    )
                    .into());
                }
                let col = state.batch.column(*i);
                Ok(SelAligned {
                    values: sel.iter().map(|r| col.get(r)).collect(),
                })
            }
        },
        Expr::Const { val, .. } => Ok(SelAligned {
            values: vec![val.clone(); sel.len()],
        }),
        Expr::Apply { op, args, span } => {
            // Children first, in evaluation order (their node ids precede
            // this node's application errors — matching row order where a
            // child's error on row r outranks this op's error on row r).
            let arg_cols: Vec<SelAligned> = args
                .iter()
                .map(|a| eval_node(a, sel, state))
                .collect::<Result<_>>()?;
            // The generic kernel: gather, apply, store. The op node's own
            // failures use a FRESH node id claimed after the children.
            let apply_node = state.node_id();
            let mut out = Vec::with_capacity(sel.len());
            let mut frame: Vec<DataValue> = Vec::with_capacity(args.len());
            for (i, row) in sel.iter().enumerate() {
                frame.clear();
                frame.extend(arg_cols.iter().map(|c| c.values[i].clone()));
                match (op.inner)(&frame) {
                    // THE columnar-lane checkpoint (row path's counterpart
                    // is `data::expr::apply_op`): a `NaN` float or
                    // vector-lane result is refused here regardless of
                    // whether the op itself remembered its own `no_nan`
                    // guard, so no op — present or future — can hand a
                    // poison value out of this evaluator either. Same typed
                    // diagnostic, same text, as if the op had refused it
                    // directly, which keeps this lane observationally
                    // identical to the row evaluator per this module's law.
                    Ok(v) if crate::data::functions::result_has_nan(&v) => {
                        use crate::data::expr::{EvalRaisedError, op_display_name};
                        use crate::data::functions::DomainError;
                        let span = *span;
                        let op_name = op_display_name(op.name);
                        #[allow(clippy::cast_possible_truncation)]
                        state.errors.offer(row as u32, apply_node, || {
                            EvalRaisedError(span, DomainError { op: op_name.into() }.to_string())
                                .into()
                        });
                        out.push(DataValue::Null);
                    }
                    Ok(v) => out.push(v),
                    Err(err) => {
                        use crate::data::expr::EvalRaisedError;
                        let span = *span;
                        #[allow(clippy::cast_possible_truncation)]
                        state.errors.offer(row as u32, apply_node, || {
                            EvalRaisedError(span, err.to_string()).into()
                        });
                        // The row is poisoned; its value is unobservable
                        // (extraction raises first), any placeholder is
                        // sound.
                        out.push(DataValue::Null);
                    }
                }
            }
            let _ = node;
            Ok(SelAligned { values: out })
        }
        Expr::Lazy { op, args, .. } => {
            // Short-circuit, columnar: rows leave the live set at their
            // deciding argument; only undecided rows reach later
            // arguments. Refused rows (type errors) record candidates and
            // leave the live set too — their placeholder value is
            // unobservable.
            let mut decided: Vec<(u32, DataValue)> = vec![];
            let mut live = sel.clone();
            for arg in args.iter() {
                if live.is_empty() {
                    break;
                }
                let vals = eval_node(arg, &live, state)?;
                let decide_node = state.node_id();
                let mut still_live = Vec::with_capacity(live.len());
                for (i, row) in live.iter().enumerate() {
                    match op.decide(&vals.values[i]) {
                        Decision::Continue => {
                            #[allow(clippy::cast_possible_truncation)]
                            still_live.push(row as u32);
                        }
                        Decision::Decided(v) => {
                            #[allow(clippy::cast_possible_truncation)]
                            decided.push((row as u32, v));
                        }
                        Decision::Refused => {
                            use crate::data::expr::PredicateTypeError;
                            let span = arg.span();
                            let val = vals.values[i].clone();
                            #[allow(clippy::cast_possible_truncation)]
                            state.errors.offer(row as u32, decide_node, || {
                                PredicateTypeError(span, val).into()
                            });
                            #[allow(clippy::cast_possible_truncation)]
                            decided.push((row as u32, DataValue::Null));
                        }
                    }
                }
                live = Selection::from_sorted(still_live);
            }
            // Undecided rows net the identity.
            for row in live.iter() {
                #[allow(clippy::cast_possible_truncation)]
                decided.push((row as u32, op.identity()));
            }
            decided.sort_by_key(|(r, _)| *r);
            debug_assert_eq!(decided.len(), sel.len());
            Ok(SelAligned {
                values: decided.into_iter().map(|(_, v)| v).collect(),
            })
        }
        Expr::Cond { clauses, .. } => {
            // Clause conditions evaluate over the still-undecided rows
            // only; a true condition routes its rows into that clause's
            // value expression. Rows surviving every clause net null.
            let mut decided: Vec<(u32, DataValue)> = vec![];
            let mut live = sel.clone();
            for (cond, val) in clauses {
                if live.is_empty() {
                    break;
                }
                let cond_vals = eval_node(cond, &live, state)?;
                let decide_node = state.node_id();
                let mut taken = Vec::with_capacity(live.len());
                let mut passed = Vec::with_capacity(live.len());
                for (i, row) in live.iter().enumerate() {
                    match cond_vals.values[i].get_bool() {
                        Some(true) => {
                            #[allow(clippy::cast_possible_truncation)]
                            taken.push(row as u32);
                        }
                        Some(false) => {
                            #[allow(clippy::cast_possible_truncation)]
                            passed.push(row as u32);
                        }
                        None => {
                            use crate::data::expr::PredicateTypeError;
                            let span = cond.span();
                            let v = cond_vals.values[i].clone();
                            #[allow(clippy::cast_possible_truncation)]
                            state.errors.offer(row as u32, decide_node, || {
                                PredicateTypeError(span, v).into()
                            });
                            #[allow(clippy::cast_possible_truncation)]
                            decided.push((row as u32, DataValue::Null));
                        }
                    }
                }
                let taken = Selection::from_sorted(taken);
                if !taken.is_empty() {
                    let arm = eval_node(val, &taken, state)?;
                    for (i, row) in taken.iter().enumerate() {
                        #[allow(clippy::cast_possible_truncation)]
                        decided.push((row as u32, arm.values[i].clone()));
                    }
                }
                live = Selection::from_sorted(passed);
            }
            for row in live.iter() {
                #[allow(clippy::cast_possible_truncation)]
                decided.push((row as u32, DataValue::Null));
            }
            decided.sort_by_key(|(r, _)| *r);
            debug_assert_eq!(decided.len(), sel.len());
            Ok(SelAligned {
                values: decided.into_iter().map(|(_, v)| v).collect(),
            })
        }
        Expr::UnboundApply { op, span, .. } => {
            use crate::data::expr::NoImplementationError;
            Err(NoImplementationError(*span, op.to_string()).into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::expr::LazyOp;
    use crate::data::functions::{OP_ADD, OP_EQ, OP_GT};
    use crate::data::value::DataValue;

    fn cnst(v: impl Into<DataValue>) -> Expr {
        Expr::Const {
            val: v.into(),
            span: Default::default(),
        }
    }
    fn binding(pos: usize) -> Expr {
        Expr::Binding {
            var: crate::data::symb::Symbol::new(format!("c{pos}"), Default::default()),
            tuple_pos: Some(pos),
        }
    }
    fn apply(op: &'static crate::data::expr::Op, args: Vec<Expr>) -> Expr {
        Expr::Apply {
            op,
            args: args.into(),
            span: Default::default(),
        }
    }

    /// THE law: columnar equals row-by-row, values and error identity
    /// both, over every (expr, batch) this generator produces.
    fn differential(expr: &Expr, rows: &[Vec<DataValue>]) {
        let width = rows.first().map_or(0, Vec::len);
        let batch = ColumnBatch::from_rows(rows, width);
        let sel = Selection::all(rows.len());
        let batched = eval_expr_batched(expr, &batch, &sel);
        // Row oracle: first error in row order wins; otherwise all values.
        let mut oracle_vals = vec![];
        let mut oracle_err: Option<String> = None;
        for row in rows {
            match expr.eval(row.as_slice()) {
                Ok(v) => oracle_vals.push(v),
                Err(e) => {
                    oracle_err = Some(e.to_string());
                    break;
                }
            }
        }
        match (batched, oracle_err) {
            (Ok(vals), None) => assert_eq!(vals, oracle_vals, "values diverge for {expr:?}"),
            (Err(be), Some(oe)) => {
                assert_eq!(be.to_string(), oe, "error identity diverges for {expr:?}");
            }
            (Ok(v), Some(oe)) => panic!("row eval errors ({oe}) but batch returned {v:?}"),
            (Err(be), None) => panic!("batch errors ({be}) but row eval is clean"),
        }
    }

    #[test]
    fn straight_line_matches_rows() {
        let rows: Vec<Vec<DataValue>> = (0..10)
            .map(|i| vec![DataValue::from(i), DataValue::from(i * 2)])
            .collect();
        differential(&apply(&OP_ADD, vec![binding(0), binding(1)]), &rows);
        differential(&apply(&OP_EQ, vec![binding(0), cnst(4)]), &rows);
    }

    #[test]
    fn guard_short_circuits_per_row() {
        // c0 == 0 rows must never evaluate the division-by-zero-ish arm;
        // an erroring op (int + string) stands in for it.
        let guard = Expr::Lazy {
            op: LazyOp::And,
            args: Box::new([
                apply(&OP_GT, vec![binding(0), cnst(0)]),
                apply(
                    &OP_GT,
                    vec![apply(&OP_ADD, vec![binding(1), cnst(1)]), cnst(0)],
                ),
            ]),
            span: Default::default(),
        };
        // Row 0: c0=0 → guard false, c1 (a string!) never touched.
        // Row 1: c0=1 → second arm runs on an int, fine.
        let rows = vec![
            vec![DataValue::from(0), DataValue::from("poison")],
            vec![DataValue::from(1), DataValue::from(5)],
        ];
        differential(&guard, &rows);
        // And the mirror: a live row DOES reach the poison.
        let rows = vec![
            vec![DataValue::from(1), DataValue::from("poison")],
            vec![DataValue::from(1), DataValue::from(5)],
        ];
        differential(&guard, &rows);
    }

    #[test]
    fn error_identity_is_first_failing_row() {
        // Rows 1 and 3 both poison; row eval reports row 1's error.
        let expr = apply(&OP_ADD, vec![binding(0), cnst(1)]);
        let rows = vec![
            vec![DataValue::from(1)],
            vec![DataValue::from("a")],
            vec![DataValue::from(2)],
            vec![DataValue::from("b")],
        ];
        differential(&expr, &rows);
    }

    #[test]
    fn cond_partitions_and_stitches_in_row_order() {
        let cond = Expr::Cond {
            clauses: vec![
                (
                    apply(&OP_GT, vec![binding(0), cnst(5)]),
                    apply(&OP_ADD, vec![binding(0), cnst(100)]),
                ),
                (cnst(true), binding(0)),
            ],
            span: Default::default(),
        };
        let rows: Vec<Vec<DataValue>> = (0..12).map(|i| vec![DataValue::from(i)]).collect();
        differential(&cond, &rows);
    }

    #[test]
    fn seeded_random_expressions_match_rows() {
        // A small structured generator: random expression trees over two
        // int-ish columns with occasional poison values, judged by the
        // differential. Deterministic LCG — no wall clock, no rand crate.
        let mut rng: u64 = 0x5EED_C011;
        fn next(rng: &mut u64) -> u64 {
            *rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *rng >> 33
        }
        fn gen_expr(rng: &mut u64, depth: usize) -> Expr {
            let choice = next(rng) % if depth == 0 { 3 } else { 6 };
            match choice {
                0 => Expr::Const {
                    val: DataValue::from((next(rng) % 7) as i64),
                    span: Default::default(),
                },
                1 | 2 => Expr::Binding {
                    var: crate::data::symb::Symbol::new("c", Default::default()),
                    tuple_pos: Some((next(rng) % 2) as usize),
                },
                3 => Expr::Apply {
                    op: if next(rng).is_multiple_of(2) {
                        &OP_ADD
                    } else {
                        &OP_GT
                    },
                    args: Box::new([gen_expr(rng, depth - 1), gen_expr(rng, depth - 1)]),
                    span: Default::default(),
                },
                4 => Expr::Lazy {
                    op: match next(rng) % 3 {
                        0 => LazyOp::And,
                        1 => LazyOp::Or,
                        _ => LazyOp::Coalesce,
                    },
                    args: Box::new([gen_expr(rng, depth - 1), gen_expr(rng, depth - 1)]),
                    span: Default::default(),
                },
                _ => Expr::Cond {
                    clauses: vec![
                        (gen_expr(rng, depth - 1), gen_expr(rng, depth - 1)),
                        (
                            Expr::Const {
                                val: DataValue::from(true),
                                span: Default::default(),
                            },
                            gen_expr(rng, depth - 1),
                        ),
                    ],
                    span: Default::default(),
                },
            }
        }
        fn gen_val(rng: &mut u64) -> DataValue {
            match next(rng) % 8 {
                0 => DataValue::from("poison"),
                1 => DataValue::Null,
                2 => DataValue::from(true),
                3 => DataValue::from(false),
                n => DataValue::from(n as i64 - 4),
            }
        }
        for _case in 0..500 {
            let expr = gen_expr(&mut rng, 3);
            let rows: Vec<Vec<DataValue>> = (0..9)
                .map(|_| vec![gen_val(&mut rng), gen_val(&mut rng)])
                .collect();
            differential(&expr, &rows);
        }
    }
}
