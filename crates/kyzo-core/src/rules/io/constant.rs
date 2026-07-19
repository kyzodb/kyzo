/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0). This is the re-homing of the `Constant` impl that lived
 * behind a seam in `parse/query.rs` (parsing cannot exist without its
 * `init_options`/`arity` surface), now with `run` — the original's `run`
 * unwrapped its way to the data (`expr_option(..).unwrap()`,
 * `get_const().unwrap()`, `get_slice().unwrap()`), trusting that
 * `init_options` had normalized the option; those unwraps are the
 * sealed [`ConstantData`] path here. Drift (arity called before
 * init_options, or the option replaced after normalization) is reported
 * as the wrong-option error instead of aborting the engine. Output rows
 * flow through the arity-checked writer.
 */

//! The fixed rule that yields a constant relation: its `data` option, a
//! list of equal-length lists, *is* the relation. It backs `<-` const
//! rules and the synthetic entry of a body-less `:create`.

use std::collections::BTreeMap;

use miette::{Diagnostic, Result, bail, ensure};
use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

use crate::data::expr::Expr;
use crate::data::program::{WrongFixedRuleOptionError, WrongFixedRuleOptionHelp};
use kyzo_model::SourceSpan;
use kyzo_model::program::symbol::Symbol;
use kyzo_model::data_value_any;
use kyzo_model::value::{DataValue, Tuple};
use crate::fixed_rule::{CancelFlag, FixedRule, FixedRuleOutput, FixedRulePayload};

pub(crate) struct Constant;

/// Rows sealed by [`Constant::init_options`]: a rectangular list-of-lists.
///
/// `arity`/`run` read only through this type's methods — they do not
/// re-validate rectangularity or list shape (P085). The sole list-row
/// proof lives in [`Self::row_cells`] (`constant_row_list`).
struct ConstantData<'a>(&'a [DataValue]);

impl ConstantData<'_> {
    /// Width of every sealed row. `None` when the relation has no rows.
    fn width(&self) -> Result<Option<usize>> {
        match self.0.first() {
            None => Ok(None),
            Some(row) => Ok(Some(Self::row_cells(row)?.len())),
        }
    }

    /// Emit every sealed row through the arity-checked writer.
    fn emit(&self, out: &mut FixedRuleOutput) -> Result<()> {
        for row in self.0 {
            out.put(Tuple::from_vec(Self::row_cells(row)?.to_vec()))?;
        }
        Ok(())
    }

    /// `init_options` sealed every element as `DataValue::List`.
    fn row_cells(row: &DataValue) -> Result<&[DataValue]> {
        match row {
            DataValue::List(cells) => Ok(cells.as_slice()),
            _ => Err(crate::fixed_rule::FixedRuleInvariantError::refuse(
                "constant_row_list",
            )),
        }
    }
}

impl Constant {
    fn wrong_option(span: SourceSpan) -> WrongFixedRuleOptionError {
        WrongFixedRuleOptionError {
            name: Symbol::new("data", span),
            span,
            rule_name: Symbol::new("Constant", span),
            help: WrongFixedRuleOptionHelp::ListOfListsRequired,
        }
    }

    /// Read the sealed `data` option. Failure means `arity`/`run` ran
    /// before `init_options`, or the option was replaced after
    /// normalization — drift, not a second validation of list shape.
    fn proven_data(
        options: &BTreeMap<SmartString<LazyCompact>, Expr>,
        span: SourceSpan,
    ) -> Result<ConstantData<'_>> {
        match options.get("data") {
            Some(Expr::Const {
                val: DataValue::List(rows),
                ..
            }) => Ok(ConstantData(rows.as_slice())),
            _ => Err(Self::wrong_option(span).into()),
        }
    }
}

impl FixedRule for Constant {
    fn run(
        &self,
        payload: FixedRulePayload<'_>,
        out: &mut FixedRuleOutput,
        _cancel: CancelFlag,
    ) -> Result<()> {
        Constant::proven_data(&payload.manifest.options, payload.span())?.emit(out)
    }

    fn arity(
        &self,
        options: &BTreeMap<SmartString<LazyCompact>, Expr>,
        rule_head: &[Symbol],
        span: SourceSpan,
    ) -> Result<usize> {
        let data = Constant::proven_data(options, span)?;
        match data.width()? {
            None => match rule_head.len() {
                0 => {
                    #[derive(Error, Debug, Diagnostic)]
                    #[error("Constant rule does not have data")]
                    #[diagnostic(code(parser::empty_const_rule))]
                    #[diagnostic(help(
                        "If you insist on using this empty rule, explicitly give its head"
                    ))]
                    struct EmptyConstRuleError(#[label] SourceSpan);
                    bail!(EmptyConstRuleError(span))
                }
                i => Ok(i),
            },
            Some(w) => Ok(w),
        }
    }

    fn init_options(
        &self,
        options: BTreeMap<SmartString<LazyCompact>, Expr>,
        span: SourceSpan,
    ) -> Result<BTreeMap<SmartString<LazyCompact>, Expr>> {
        let mut options = options;
        let data = options
            .get("data")
            .ok_or_else(|| Self::wrong_option(span))?;
        let data = match data.clone().eval_to_const()? {
            DataValue::List(l) => l,
            data_value_any!() => bail!(Self::wrong_option(span)),
        };

        let mut tuples = vec![];
        let mut last_len = None;
        for row in data {
            match row {
                DataValue::List(tuple) => {
                    if let Some(l) = &last_len {
                        #[derive(Error, Debug, Diagnostic)]
                        #[error("Constant head must have the same arity as the data given")]
                        #[diagnostic(code(parser::const_data_arity_mismatch))]
                        #[diagnostic(help("First row length: {0}; the mismatch: {1:?}"))]
                        struct ConstRuleRowArityMismatch(
                            usize,
                            Vec<DataValue>,
                            #[label] SourceSpan,
                        );

                        ensure!(
                            *l == tuple.len(),
                            ConstRuleRowArityMismatch(*l, tuple, span)
                        );
                    };
                    last_len = Some(tuple.len());
                    tuples.push(DataValue::List(tuple));
                }
                row @ (data_value_any!()) => {
                    #[derive(Error, Debug, Diagnostic)]
                    #[error("Bad row for constant rule: {0:?}")]
                    #[diagnostic(code(parser::bad_row_for_const))]
                    #[diagnostic(help(
                        "The body of a constant rule should evaluate to a list of lists"
                    ))]
                    struct ConstRuleRowNotList(DataValue, #[label("not a list")] SourceSpan);

                    bail!(ConstRuleRowNotList(row, span))
                }
            }
        }

        options.insert(
            SmartString::from("data"),
            Expr::Const {
                val: DataValue::List(tuples),
                span,
            },
        );
        Ok(options)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kyzo_model::value::Tuple;
    use crate::fixed_rule::tests_support::run_fixed_rule;

    /// `init_options` normalizes, `arity` reads the proof, `run` emits the
    /// rows (the harness drives all three in order, as parse/eval do).
    #[test]
    fn constant_round_trip() {
        let options = BTreeMap::from([(
            SmartString::from("data"),
            Expr::Const {
                val: DataValue::List(vec![
                    DataValue::List(vec![DataValue::from(1i64), DataValue::from("x")]),
                    DataValue::List(vec![DataValue::from(2i64), DataValue::from("y")]),
                ]),
                span: SourceSpan::default(),
            },
        )]);
        let got = run_fixed_rule(&Constant, vec![], options, CancelFlag::default()).unwrap();
        assert_eq!(got.len(), 2);
        let want: Tuple = Tuple::from_vec(vec![DataValue::from(1i64), DataValue::from("x")]);
        assert_eq!(got[0], want);
    }

    /// Un-normalized (or drifted) options are a typed refusal in `run`,
    /// not an abort: the original unwrapped here.
    #[test]
    fn drifted_options_refuse_typed() {
        // `arity` before `init_options`, with a non-const option: refused.
        let options = BTreeMap::from([(
            SmartString::from("data"),
            Expr::Const {
                val: DataValue::from("not a list"),
                span: SourceSpan::default(),
            },
        )]);
        let err = Constant
            .arity(&options, &[], SourceSpan::default())
            .unwrap_err();
        assert!(err.to_string().contains("Wrong value"), "{err}");

        // Ragged rows are refused at normalization.
        let options = BTreeMap::from([(
            SmartString::from("data"),
            Expr::Const {
                val: DataValue::List(vec![
                    DataValue::List(vec![DataValue::from(1i64)]),
                    DataValue::List(vec![DataValue::from(1i64), DataValue::from(2i64)]),
                ]),
                span: SourceSpan::default(),
            },
        )]);
        let err = Constant
            .init_options(options, SourceSpan::default())
            .unwrap_err();
        assert!(err.to_string().contains("same arity"), "{err}");
    }

    #[test]
    fn constant_data_empty_has_no_width() {
        assert_eq!(ConstantData(&[]).width().unwrap(), None);
    }
}
