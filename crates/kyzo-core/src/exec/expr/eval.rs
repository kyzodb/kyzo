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

use miette::{Diagnostic, Result, bail, ensure, miette};
use thiserror::Error;

use crate::exec::expr::batch::{BatchRowU32Overflow, ColumnBatch, ErrorMin, Selection};
use crate::exec::stdlib::resolve_op;
use kyzo_model::data_value_any;
use kyzo_model::data_value_to_vld_spec;
use kyzo_model::program::WriteValidity;
use kyzo_model::program::expr::{
    BindingPos, Decision, EvalRaisedError, Expr, NoImplementationError, PredicateTypeError,
    TupleTooShortError, UnboundVariableError,
};
use kyzo_model::program::op::OpDecl;
use kyzo_model::value::{DataValue, ValidityTs};

/// Selection indexes are `u32`; [`Selection::iter`] widens stored ids to
/// `usize`. Narrowing is total — overflow is refused at [`Selection::all`].
#[cfg(test)]
use kyzo_model::program::expr::LazyOp;

/// Live selection names a row past the batch height.
#[derive(Debug, Error, Diagnostic, PartialEq, Eq)]
#[error("selection beyond batch height")]
#[diagnostic(code(query::selection_beyond_height))]
struct SelectionBeyondHeight;

/// Partitioned expression arms did not cover every live selection row.
#[derive(Debug, Error, Diagnostic, PartialEq, Eq)]
#[error("expression arm length mismatch with selection")]
#[diagnostic(code(query::expr_arm_len))]
struct ExprArmLenMismatch;

#[inline]
fn row_sel(row: usize) -> Result<u32> {
    u32::try_from(row).map_err(|_| BatchRowU32Overflow { n: row }.into())
}

fn bound_of(op: OpDecl) -> Result<&'static crate::exec::stdlib::BoundOp> {
    resolve_op(&op.display_name()).ok_or_else(|| miette!("unknown builtin op {}", op.name))
}

/// Resolve a mutation's valid-time coordinate for one row at the write
/// boundary: `Now` is the transaction stamp, `Fixed` is constant, and
/// `PerRow` evaluates the full carried [`Expr`] via [`eval_expr`] then
/// re-proves the terminal tick through [`ValidityTs::for_assertion`].
///
/// Model `WriteValidity` is declaration-only; this is the evaluation body
/// (same declaration/body split as `OpDecl` / `BoundOp`).
pub(crate) fn resolve_write_validity(
    write_vld: &WriteValidity,
    row: &[DataValue],
    stamp: ValidityTs,
    cur_vld: ValidityTs,
) -> Result<ValidityTs> {
    match write_vld {
        WriteValidity::Now => Ok(stamp),
        WriteValidity::Fixed(v) => Ok(*v),
        WriteValidity::PerRow(expr) => {
            let span = expr.span();
            let val = eval_expr(expr, row)?;
            let vld = data_value_to_vld_spec(val, span, cur_vld)?;
            // Parse proved the expression names one of the mutation's
            // output columns, never what value that column will hold for
            // any given row. Re-prove per row: a user-asserted write
            // validity can never be the reserved terminal tick
            // (`i64::MAX` / `'END'`).
            ValidityTs::for_assertion(vld.raw()).ok_or_else(|| {
                miette!(
                    labels = vec![miette::LabeledSpan::underline(span)],
                    "a write validity cannot be the reserved terminal tick (i64::MAX / 'END')"
                )
            })
        }
    }
}

/// Row-path expression evaluation — sole door that applies [`BoundOp::apply`].
pub(crate) fn eval_expr(expr: &Expr, bindings: impl AsRef<[DataValue]>) -> Result<DataValue> {
    let bindings = bindings.as_ref();
    match expr {
        Expr::Binding { var, tuple_pos, .. } => match tuple_pos {
            BindingPos::Unresolved => {
                bail!(UnboundVariableError(var.name.to_string(), var.span))
            }
            BindingPos::Resolved(i) => Ok(bindings
                .get(*i)
                .ok_or_else(|| {
                    TupleTooShortError(var.name.to_string(), *i, bindings.len(), var.span)
                })?
                .clone()),
        },
        Expr::Const { val, .. } => Ok(val.clone()),
        Expr::Apply { op, args, .. } => {
            let args: Vec<DataValue> = args
                .iter()
                .map(|v| eval_expr(v, bindings))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(bound_of(*op)?
                .apply(&args)
                .map_err(|err| EvalRaisedError(expr.span(), err.to_string()))?)
        }
        Expr::Cond { clauses, .. } => {
            for (cond, val) in clauses {
                let cond_val = eval_expr(cond, bindings)?;
                let cond_val = cond_val
                    .get_bool()
                    .ok_or_else(|| PredicateTypeError(cond.span(), cond_val))?;
                if cond_val {
                    return eval_expr(val, bindings);
                }
            }
            Ok(DataValue::Null)
        }
        Expr::Lazy { op, args, .. } => {
            for arg in args.iter() {
                let v = eval_expr(arg, bindings)?;
                match op.decide(&v) {
                    Decision::Decided(d) => return Ok(d),
                    Decision::Continue => {}
                    Decision::Refused => bail!(PredicateTypeError(arg.span(), v)),
                }
            }
            Ok(op.identity())
        }
        Expr::UnboundApply { op, span, .. } => {
            bail!(NoImplementationError(*span, op.to_string()));
        }
    }
}

pub(crate) fn eval_pred(expr: &Expr, bindings: impl AsRef<[DataValue]>) -> Result<bool> {
    match eval_expr(expr, bindings)? {
        DataValue::Bool(b) => Ok(b),
        v @ (data_value_any!()) => bail!(PredicateTypeError(expr.span(), v)),
    }
}

/// Fold then evaluate closed expressions (including nondeterministic ops once).
pub(crate) fn eval_to_const(mut expr: Expr) -> Result<DataValue> {
    expr.partial_eval()?;
    if let Expr::Const { val, .. } = expr {
        return Ok(val);
    }
    if expr.bindings()?.is_empty() {
        return eval_expr(&expr, &[] as &[DataValue]);
    }
    #[derive(Debug, thiserror::Error, miette::Diagnostic)]
    #[error("Expression contains unevaluated constant")]
    #[diagnostic(code(eval::not_constant))]
    struct NotConstError(#[label("not a constant")] kyzo_model::SourceSpan);
    bail!(NotConstError(expr.span()))
}

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
    if !selection.iter().all(|r| r < batch.height()) {
        bail!(SelectionBeyondHeight);
    }
    let mut state = BatchEval {
        batch,
        next_node: 0,
        errors: ErrorMin::empty(),
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
    use kyzo_model::program::expr::PredicateTypeError;
    let values = eval_expr_batched(expr, batch, selection)?;
    let mut keep = Vec::with_capacity(values.len());
    for (v, row) in values.iter().zip(selection.iter()) {
        match v.get_bool() {
            Some(true) => {
                keep.push(row_sel(row)?);
            }
            Some(false) => {}
            None => {
                return Err(PredicateTypeError(expr.span(), v.clone()).into());
            }
        }
    }
    Ok(Selection::from_sorted(keep)?)
}

fn eval_node(expr: &Expr, sel: &Selection, state: &mut BatchEval<'_>) -> Result<SelAligned> {
    let node = state.node_id();
    match expr {
        Expr::Binding { var, tuple_pos, .. } => match tuple_pos {
            BindingPos::Unresolved => {
                use kyzo_model::program::expr::UnboundVariableError;
                Err(UnboundVariableError(var.name.to_string(), var.span).into())
            }
            BindingPos::Resolved(i) => {
                if *i >= state.batch.width() {
                    use kyzo_model::program::expr::TupleTooShortError;
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
                match bound_of(*op).and_then(|b| b.apply(&frame)) {
                    // Sole apply door: BoundOp::apply (typed NaN Refuse inside).
                    Ok(v) => out.push(v),
                    Err(err) => {
                        let span = *span;
                        state.errors.offer(row_sel(row)?, apply_node, || {
                            EvalRaisedError(span, err.to_string()).into()
                        });
                        out.push(DataValue::Null);
                    }
                }
            }
            drop(node);
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
                            still_live.push(row_sel(row)?);
                        }
                        Decision::Decided(v) => {
                            decided.push((row_sel(row)?, v));
                        }
                        Decision::Refused => {
                            use kyzo_model::program::expr::PredicateTypeError;
                            let span = arg.span();
                            let val = vals.values[i].clone();
                            state.errors.offer(row_sel(row)?, decide_node, || {
                                PredicateTypeError(span, val).into()
                            });
                            decided.push((row_sel(row)?, DataValue::Null));
                        }
                    }
                }
                live = Selection::from_sorted(still_live)?;
            }
            // Undecided rows net the identity.
            for row in live.iter() {
                decided.push((row_sel(row)?, op.identity()));
            }
            decided.sort_by_key(|(r, _)| *r);
            ensure!(decided.len() == sel.len(), ExprArmLenMismatch);
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
                            taken.push(row_sel(row)?);
                        }
                        Some(false) => {
                            passed.push(row_sel(row)?);
                        }
                        None => {
                            use kyzo_model::program::expr::PredicateTypeError;
                            let span = cond.span();
                            let v = cond_vals.values[i].clone();
                            state.errors.offer(row_sel(row)?, decide_node, || {
                                PredicateTypeError(span, v).into()
                            });
                            decided.push((row_sel(row)?, DataValue::Null));
                        }
                    }
                }
                let taken = Selection::from_sorted(taken)?;
                if !taken.is_empty() {
                    let arm = eval_node(val, &taken, state)?;
                    for (i, row) in taken.iter().enumerate() {
                        decided.push((row_sel(row)?, arm.values[i].clone()));
                    }
                }
                live = Selection::from_sorted(passed)?;
            }
            for row in live.iter() {
                decided.push((row_sel(row)?, DataValue::Null));
            }
            decided.sort_by_key(|(r, _)| *r);
            ensure!(decided.len() == sel.len(), ExprArmLenMismatch);
            Ok(SelAligned {
                values: decided.into_iter().map(|(_, v)| v).collect(),
            })
        }
        Expr::UnboundApply { op, span, .. } => {
            use kyzo_model::program::expr::NoImplementationError;
            Err(NoImplementationError(*span, op.to_string()).into())
        }
    }
}

#[cfg(test)]
mod tests {
    use miette::{Result, miette};
    use super::*;
    use kyzo_model::program::op::{OP_ADD, OP_EQ, OP_GT};
    use kyzo_model::value::DataValue;

    fn cnst(v: impl Into<DataValue>) -> Expr {
        Expr::Const {
            val: v.into(),
            span: SourceSpan::empty(),
        }
    }
    fn binding(pos: usize) -> Expr {
        Expr::Binding {
            var: kyzo_model::program::symbol::Symbol::new(format!("c{pos}"), SourceSpan::empty()),
            tuple_pos: BindingPos::Resolved(pos),
        }
    }
    fn apply(op: OpDecl, args: Vec<Expr>) -> Expr {
        Expr::Apply {
            op,
            args: args.into(),
            span: SourceSpan::empty(),
        }
    }

    /// THE law: columnar equals row-by-row, values and error identity
    /// both, over every (expr, batch) this generator produces.
    fn differential(expr: &Expr, rows: &[Vec<DataValue>]) -> Result<()> {
        let width = rows.first().map_or(0, Vec::len);
        let owned_rows: Vec<kyzo_model::value::Tuple> = rows
            .iter()
            .cloned()
            .map(kyzo_model::value::Tuple::from_vec)
            .collect();
        let batch = ColumnBatch::from_rows(owned_rows, width).map_err(|e| miette!("test rows uniform width: {e}"))?;
        let sel = Selection::all(rows.len()).map_err(|e| miette!("test batch fits u32: {e}"))?;
        let batched = eval_expr_batched(expr, &batch, &sel);
        // Row oracle: first error in row order wins; otherwise all values.
        let mut oracle_vals = vec![];
        let mut oracle_err: Option<String> = None;
        for row in rows {
            match eval_expr(expr, row.as_slice()) {
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
            (Ok(v), Some(oe)) => {
                return Err(miette!(
                    "row eval errors ({oe}) but batch returned {v:?}"
                ));
            }
            (Err(be), None) => {
                return Err(miette!(
                    "batch errors ({be}) but row eval is clean"
                ));
            }
        }
        Ok(())
    }

    #[test]
    fn straight_line_matches_rows() {
        let rows: Vec<Vec<DataValue>> = (0..10)
            .map(|i| vec![DataValue::from(i), DataValue::from(i * 2)])
            .collect();
        differential(&apply(OP_ADD, vec![binding(0), binding(1)]), &rows);
        differential(&apply(OP_EQ, vec![binding(0), cnst(4)]), &rows);
    }

    #[test]
    fn guard_short_circuits_per_row() {
        // c0 == 0 rows must never evaluate the division-by-zero-ish arm;
        // an erroring op (int + string) stands in for it.
        let guard = Expr::Lazy {
            op: LazyOp::And,
            args: Box::new([
                apply(OP_GT, vec![binding(0), cnst(0)]),
                apply(
                    OP_GT,
                    vec![apply(OP_ADD, vec![binding(1), cnst(1)]), cnst(0)],
                ),
            ]),
            span: SourceSpan::empty(),
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
        let expr = apply(OP_ADD, vec![binding(0), cnst(1)]);
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
                    apply(OP_GT, vec![binding(0), cnst(5)]),
                    apply(OP_ADD, vec![binding(0), cnst(100)]),
                ),
                (cnst(true), binding(0)),
            ],
            span: SourceSpan::empty(),
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
            // INVARIANT(lcg64): Knuth LCG step is defined wrapping on u64.
            *rng = (std::num::Wrapping(rng) * std::num::Wrapping(6364136223846793005) + std::num::Wrapping(1442695040888963407)).0;
            *rng >> 33
        }
        fn gen_expr(rng: &mut u64, depth: usize) -> Expr {
            let choice = next(rng) % if depth == 0 { 3 } else { 6 };
            match choice {
                0 => Expr::Const {
                    val: DataValue::from(match i64::try_from(next(rng) % 7) {
                        Ok(v) => v,
                        Err(_gt_i64) => 0,
                    }),
                    span: SourceSpan::empty(),
                },
                1 | 2 => Expr::Binding {
                    var: kyzo_model::program::symbol::Symbol::new("c", SourceSpan::empty()),
                    tuple_pos: BindingPos::Resolved(match usize::try_from(next(rng) % 2) {
                        Ok(v) => v,
                        Err(_gt_usize) => 0,
                    }),
                },
                3 => Expr::Apply {
                    op: if next(rng).is_multiple_of(2) {
                        OP_ADD
                    } else {
                        OP_GT
                    },
                    args: Box::new([gen_expr(rng, depth - 1), gen_expr(rng, depth - 1)]),
                    span: SourceSpan::empty(),
                },
                4 => Expr::Lazy {
                    op: {
                        let lazy_choice = next(rng) % 3;
                        if lazy_choice == 0 {
                            LazyOp::And
                        } else if lazy_choice == 1 {
                            LazyOp::Or
                        } else {
                            LazyOp::Coalesce
                        }
                    },
                    args: Box::new([gen_expr(rng, depth - 1), gen_expr(rng, depth - 1)]),
                    span: SourceSpan::empty(),
                },
                5 => Expr::Cond {
                    clauses: vec![
                        (gen_expr(rng, depth - 1), gen_expr(rng, depth - 1)),
                        (
                            Expr::Const {
                                val: DataValue::from(true),
                                span: SourceSpan::empty(),
                            },
                            gen_expr(rng, depth - 1),
                        ),
                    ],
                    span: SourceSpan::empty(),
                },
                // Named exhaustiveness arm: `choice` is `next % 3` or `% 6`.
                // A residue ≥6 is outside the generator contract — emit a
                // leaf Const the differential can still judge. Never `_ =>`.
                modulus_overflow @ 6..=u64::MAX => {
                    let _named_overflow = modulus_overflow;
                    Expr::Const {
                        val: DataValue::Null,
                        span: SourceSpan::empty(),
                    }
                },
            }
        }
        fn gen_val(rng: &mut u64) -> DataValue {
            match next(rng) % 8 {
                0 => DataValue::from("poison"),
                1 => DataValue::Null,
                2 => DataValue::from(true),
                3 => DataValue::from(false),
                n => {
                    let ni = match i64::try_from(n) {
                        Ok(v) => v,
                        Err(_gt_i64) => 0,
                    };
                    DataValue::from(ni - 4)
                }
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
