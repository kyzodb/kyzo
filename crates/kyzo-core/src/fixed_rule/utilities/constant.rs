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
 * `proven_data` path here, and drift (arity called before init_options,
 * or the option replaced after normalization) is reported as the
 * wrong-option error instead of aborting the engine. Output rows flow
 * through the arity-checked writer.
 */

//! The fixed rule that yields a constant relation: its `data` option, a
//! list of equal-length lists, *is* the relation. It backs `<-` const
//! rules and the synthetic entry of a body-less `:create`.

use std::collections::BTreeMap;

use miette::{Diagnostic, Result, bail, ensure};
use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

use crate::data::expr::Expr;
use crate::data::program::WrongFixedRuleOptionError;
use crate::data::span::SourceSpan;
use crate::data::symb::Symbol;
use crate::data::value::{DataValue, Tuple};
use crate::fixed_rule::{CancelFlag, FixedRule, FixedRuleOutput, FixedRulePayload};
use crate::data::value::data_value_any;

pub(crate) struct Constant;

impl Constant {
    /// The `data` option as `init_options` proved it: a constant list.
    /// Failure here means `arity`/`run` was called before `init_options`,
    /// or the option was replaced after normalization — drift, reported as
    /// the same wrong-option error rather than an abort.
    fn proven_data(
        options: &BTreeMap<SmartString<LazyCompact>, Expr>,
        span: SourceSpan,
    ) -> Result<&[DataValue]> {
        options
            .get("data")
            .and_then(|d| d.get_const())
            .and_then(|v| v.get_slice())
            .ok_or_else(|| {
                WrongFixedRuleOptionError {
                    name: "data".to_string(),
                    span,
                    rule_name: "Constant".to_string(),
                    help: "a list of lists is required".to_string(),
                }
                .into()
            })
    }
}

impl FixedRule for Constant {
    fn run(
        &self,
        payload: FixedRulePayload<'_>,
        out: &mut FixedRuleOutput,
        _cancel: CancelFlag,
    ) -> Result<()> {
        let data = Constant::proven_data(&payload.manifest.options, payload.span())?;
        for row in data {
            // `init_options` proved every row is a list; a non-list here
            // is drift, reported as the wrong-option error.
            let tuple = Tuple::from_vec(
                row.get_slice()
                    .ok_or_else(|| WrongFixedRuleOptionError {
                        name: "data".to_string(),
                        span: payload.span(),
                        rule_name: "Constant".to_string(),
                        help: "a list of lists is required".to_string(),
                    })?
                    .to_vec(),
            );
            out.put(tuple)?
        }
        Ok(())
    }

    fn arity(
        &self,
        options: &BTreeMap<SmartString<LazyCompact>, Expr>,
        rule_head: &[Symbol],
        span: SourceSpan,
    ) -> Result<usize> {
        let data = Constant::proven_data(options, span)?;
        match data.first() {
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
            Some(first_row) => first_row
                .get_slice()
                .map(|s| s.len())
                // `init_options` proved every row is a list; a non-list
                // here is drift, reported as the wrong-option error.
                .ok_or_else(|| {
                    WrongFixedRuleOptionError {
                        name: "data".to_string(),
                        span,
                        rule_name: "Constant".to_string(),
                        help: "a list of lists is required".to_string(),
                    }
                    .into()
                }),
        }
    }

    fn init_options(
        &self,
        options: &mut BTreeMap<SmartString<LazyCompact>, Expr>,
        span: SourceSpan,
    ) -> Result<()> {
        let wrong_option = || WrongFixedRuleOptionError {
            name: "data".to_string(),
            span,
            rule_name: "Constant".to_string(),
            help: "a list of lists is required".to_string(),
        };
        let data = options.get("data").ok_or_else(wrong_option)?;
        let data = match data.clone().eval_to_const()? {
            DataValue::List(l) => l,
            data_value_any!() => bail!(wrong_option()),
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

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::value::Tuple;
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
        let mut options = BTreeMap::from([(
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
            .init_options(&mut options, SourceSpan::default())
            .unwrap_err();
        assert!(err.to_string().contains("same arity"), "{err}");
    }
}
