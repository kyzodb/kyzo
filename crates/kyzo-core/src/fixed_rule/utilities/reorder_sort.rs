/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0): `Expr::Apply`'s op is now `&'static Op` (the original held
 * an `Arc`), so the `OP_LIST` matches deref accordingly; the
 * `val.last()` in the ranking loop is `INVARIANT(reorder_sort_key)`
 * (every buffered tuple ends with its sort key, pushed above); output
 * rows flow through the arity-checked writer.
 */

//! `ReorderSort`: evaluate `out` expressions over each input row, sort by
//! `sort_by`, and emit rank-prefixed rows with skip/take/tie control.

use std::collections::BTreeMap;

use itertools::Itertools;
use miette::{Result, bail};
use smartstring::{LazyCompact, SmartString};

use crate::data::expr::{BindingPos, Expr};
use crate::data::functions::OP_LIST;
use crate::data::program::WrongFixedRuleOptionError;
use crate::data::span::SourceSpan;
use crate::data::symb::Symbol;
use crate::data::value::DataValue;
use crate::data::value::Tuple;
use crate::fixed_rule::{
    CancelFlag, CannotDetermineArity, FixedRule, FixedRuleOutput, FixedRulePayload,
};

pub(crate) struct ReorderSort;

impl FixedRule for ReorderSort {
    fn run(
        &self,
        payload: FixedRulePayload<'_>,
        out: &mut FixedRuleOutput,
        cancel: CancelFlag,
    ) -> Result<()> {
        let in_rel = payload.get_input(0)?;

        let mut out_list = match payload.expr_option("out", None)? {
            Expr::Const {
                val: DataValue::List(l),
                span,
            } => l
                .iter()
                .map(|d| Expr::Const {
                    val: d.clone(),
                    span,
                })
                .collect_vec(),
            Expr::Apply { op, args, .. } if *op == OP_LIST => args.to_vec(),
            Expr::Binding { .. } | Expr::Const { .. } | Expr::Apply { .. } | Expr::UnboundApply { .. } | Expr::Cond { .. } | Expr::Lazy { .. } => {
                bail!(WrongFixedRuleOptionError {
                    name: "out".to_string(),
                    span: payload.span(),
                    rule_name: payload.name().to_string(),
                    help: "This option must evaluate to a list".to_string()
                })
            }
        };

        let mut sort_by = payload.expr_option(
            "sort_by",
            Some(Expr::Const {
                val: DataValue::Null,
                span: SourceSpan(0, 0),
            }),
        )?;
        let sort_descending = payload.bool_option("descending", Some(false))?;
        let break_ties = payload.bool_option("break_ties", Some(false))?;
        let skip = payload.non_neg_integer_option("skip", Some(0))?;
        let take = payload.non_neg_integer_option("take", Some(0))?;

        let binding_map = in_rel.get_binding_map(0);
        sort_by.fill_binding_indices(&binding_map)?;
        for out in out_list.iter_mut() {
            out.fill_binding_indices(&binding_map)?;
        }
        let mut buffer = vec![];
        for tuple in in_rel.iter()? {
            let tuple = tuple?;
            let sorter = sort_by.eval(&tuple)?;
            let mut s_tuple: Vec<_> = out_list.iter().map(|ex| ex.eval(&tuple)).try_collect()?;
            s_tuple.push(sorter);
            buffer.push(s_tuple);
            cancel.check()?;
        }
        if sort_descending {
            buffer.sort_by(|l, r| r.last().cmp(&l.last()));
        } else {
            buffer.sort_by(|l, r| l.last().cmp(&r.last()));
        }

        let mut count = 0usize;
        let mut rank = 0usize;
        let mut last: Option<&DataValue> = None;
        let take_plus_skip = take.saturating_add(skip);
        for val in &buffer {
            // INVARIANT(reorder_sort_key): every buffered tuple ends with
            // the sort key pushed above, so it is non-empty.
            let sorter = val
                .last()
                .expect("INVARIANT(reorder_sort_key): buffered tuple has sort key");

            if last == Some(sorter) {
                count += 1;
            } else {
                count += 1;
                rank = count;
                last = Some(sorter);
            }

            if take != 0 && count > take_plus_skip {
                break;
            }

            if count <= skip {
                continue;
            }
            let mut out_t: Tuple =
                Tuple::from_vec(vec![DataValue::from(
                    if break_ties { count } else { rank } as i64
                )]);
            out_t.extend(val[0..val.len() - 1].iter().cloned());
            out.put(out_t)?;
            cancel.check()?;
        }
        Ok(())
    }

    fn arity(
        &self,
        opts: &BTreeMap<SmartString<LazyCompact>, Expr>,
        _rule_head: &[Symbol],
        span: SourceSpan,
    ) -> Result<usize> {
        let out_opts = opts.get("out").ok_or_else(|| {
            CannotDetermineArity(
                "ReorderSort".to_string(),
                "option 'out' not provided".to_string(),
                span,
            )
        })?;
        Ok(match out_opts {
            Expr::Const {
                val: DataValue::List(l),
                ..
            } => l.len() + 1,
            Expr::Apply { op, args, .. } if **op == OP_LIST => args.len() + 1,
            Expr::Binding { .. } | Expr::Const { .. } | Expr::Apply { .. } | Expr::UnboundApply { .. } | Expr::Cond { .. } | Expr::Lazy { .. } => bail!(CannotDetermineArity(
                "ReorderSort".to_string(),
                "invalid option 'out' given, expect a list".to_string(),
                span
            )),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixed_rule::CancelFlag;
    use crate::fixed_rule::tests_support::{TestInput, run_fixed_rule};

    fn s(v: &str) -> DataValue {
        DataValue::from(v)
    }

    fn binding(name: &'static str) -> Expr {
        Expr::Binding {
            var: Symbol::new(name, SourceSpan::default()),
            tuple_pos: BindingPos::Unresolved,
        }
    }

    fn c(val: DataValue) -> Expr {
        Expr::Const {
            val,
            span: SourceSpan::default(),
        }
    }

    /// `out: [x], sort_by: x` over one column of mixed types.
    fn opts(extra: &[(&str, DataValue)]) -> BTreeMap<SmartString<LazyCompact>, Expr> {
        let mut m = BTreeMap::from([
            (
                SmartString::from("out"),
                Expr::Apply {
                    op: &OP_LIST,
                    args: Box::new([binding("id")]),
                    span: SourceSpan::default(),
                },
            ),
            (SmartString::from("sort_by"), binding("k")),
        ]);
        for (name, val) in extra {
            m.insert(SmartString::from(*name), c(val.clone()));
        }
        m
    }

    /// VALUE ORACLE: mixed-type sort keys order per the `DataValue` total
    /// order — Null < Bool < Num < Str, with ints and floats compared
    /// numerically inside Num (0.5 < 1). Keys are distinct, so ranks are
    /// 1..=5 in that order, each row (rank, id).
    #[test]
    fn sorts_mixed_types_per_datavalue_order() {
        let rows: Vec<Tuple> = vec![
            Tuple::from_vec(vec![s("n"), DataValue::Null]),
            Tuple::from_vec(vec![s("b"), DataValue::from(false)]),
            Tuple::from_vec(vec![s("h"), DataValue::from(0.5)]),
            Tuple::from_vec(vec![s("i"), DataValue::from(1i64)]),
            Tuple::from_vec(vec![s("s"), DataValue::from("str")]),
        ];
        let got = run_fixed_rule(
            &ReorderSort,
            vec![TestInput::new(vec!["id", "k"], rows)],
            opts(&[]),
            CancelFlag::default(),
        )
        .unwrap();
        let i = |v: i64| DataValue::from(v);
        let want: Vec<Tuple> = vec![
            Tuple::from_vec(vec![i(1), s("n")]), // Null
            Tuple::from_vec(vec![i(2), s("b")]), // Bool(false)
            Tuple::from_vec(vec![i(3), s("h")]), // Num(0.5)
            Tuple::from_vec(vec![i(4), s("i")]), // Num(1)
            Tuple::from_vec(vec![i(5), s("s")]), // Str
        ];
        assert_eq!(got, want);
    }

    /// VALUE ORACLE for the rank semantics around ties, descending order,
    /// and skip/take. Input (id, k): (a,1) (b,1) (c,2).
    ///
    /// Hand computation:
    ///   default:            a and b tie at rank 1, c lands at rank 3
    ///                       (competition ranking — rank skips to count)
    ///   break_ties: true:   ranks become the running count 1, 2, 3
    ///   descending: true:   c first at rank 1; a, b tie at rank 2 (the
    ///                       sort is stable, but tied rows share the rank
    ///                       so store order pins the rows regardless)
    #[test]
    fn rank_ties_and_descending() {
        let rows = || -> Vec<Tuple> {
            vec![
                Tuple::from_vec(vec![s("a"), DataValue::from(1i64)]),
                Tuple::from_vec(vec![s("b"), DataValue::from(1i64)]),
                Tuple::from_vec(vec![s("c"), DataValue::from(2i64)]),
            ]
        };
        let i = |v: i64| DataValue::from(v);

        let got = run_fixed_rule(
            &ReorderSort,
            vec![TestInput::new(vec!["id", "k"], rows())],
            opts(&[]),
            CancelFlag::default(),
        )
        .unwrap();
        let want: Vec<Tuple> = vec![
            Tuple::from_vec(vec![i(1), s("a")]),
            Tuple::from_vec(vec![i(1), s("b")]),
            Tuple::from_vec(vec![i(3), s("c")]),
        ];
        assert_eq!(got, want);

        let got = run_fixed_rule(
            &ReorderSort,
            vec![TestInput::new(vec!["id", "k"], rows())],
            opts(&[("break_ties", DataValue::from(true))]),
            CancelFlag::default(),
        )
        .unwrap();
        let want: Vec<Tuple> = vec![
            Tuple::from_vec(vec![i(1), s("a")]),
            Tuple::from_vec(vec![i(2), s("b")]),
            Tuple::from_vec(vec![i(3), s("c")]),
        ];
        assert_eq!(got, want);

        let got = run_fixed_rule(
            &ReorderSort,
            vec![TestInput::new(vec!["id", "k"], rows())],
            opts(&[("descending", DataValue::from(true))]),
            CancelFlag::default(),
        )
        .unwrap();
        let want: Vec<Tuple> = vec![
            Tuple::from_vec(vec![i(1), s("c")]),
            Tuple::from_vec(vec![i(2), s("a")]),
            Tuple::from_vec(vec![i(2), s("b")]),
        ];
        assert_eq!(got, want);
    }

    /// VALUE ORACLE for skip/take: distinct keys (a,1) (b,2) (c,3) with
    /// skip: 1, take: 1 — a is skipped (count 1 ≤ skip), b is emitted at
    /// its overall rank 2, and c breaks the loop (count 3 > take + skip).
    #[test]
    fn skip_take_window() {
        let got = run_fixed_rule(
            &ReorderSort,
            vec![TestInput::new(
                vec!["id", "k"],
                vec![
                    Tuple::from_vec(vec![s("a"), DataValue::from(1i64)]),
                    Tuple::from_vec(vec![s("b"), DataValue::from(2i64)]),
                    Tuple::from_vec(vec![s("c"), DataValue::from(3i64)]),
                ],
            )],
            opts(&[
                ("skip", DataValue::from(1i64)),
                ("take", DataValue::from(1i64)),
            ]),
            CancelFlag::default(),
        )
        .unwrap();
        let want: Vec<Tuple> = vec![Tuple::from_vec(vec![DataValue::from(2i64), s("b")])];
        assert_eq!(got, want);
    }
}
