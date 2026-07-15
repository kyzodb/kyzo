/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0): output rows flow through the arity-checked writer; otherwise
 * unchanged.
 */

//! Degree centrality: total, out- and in-degree per node, straight off the
//! edge tuples (no graph materialization).

use std::collections::BTreeMap;

use miette::Result;
use smartstring::{LazyCompact, SmartString};

use crate::data::expr::Expr;
use crate::data::span::SourceSpan;
use crate::data::symb::Symbol;
use crate::data::value::{DataValue, Tuple};
use crate::fixed_rule::{CancelFlag, FixedRule, FixedRuleOutput, FixedRulePayload};

pub(crate) struct DegreeCentrality;

impl FixedRule for DegreeCentrality {
    fn run(
        &self,
        payload: FixedRulePayload<'_>,
        out: &mut FixedRuleOutput,
        cancel: CancelFlag,
    ) -> Result<()> {
        let it = payload.get_input(0)?.ensure_min_len(2)?.iter()?;
        let mut counter: BTreeMap<DataValue, (usize, usize, usize)> = BTreeMap::new();
        for tuple in it {
            let tuple = tuple?;
            let from = tuple[0].clone();
            let (from_total, from_out, _) = counter.entry(from).or_default();
            *from_total += 1;
            *from_out += 1;

            let to = tuple[1].clone();
            let (to_total, _, to_in) = counter.entry(to).or_default();
            *to_total += 1;
            *to_in += 1;
            cancel.check()?;
        }
        if let Ok(nodes) = payload.get_input(1) {
            // A missing (unbound) nodes relation is the "not provided" case
            // above and skips this block entirely; a PROVIDED nullary
            // relation is a real error, not something to silently ignore —
            // propagate it instead of letting `tuple[0]` panic below.
            let nodes = nodes.ensure_min_len(1)?;
            for tuple in nodes.iter()? {
                let tuple = tuple?;
                // Structural: `ensure_min_len(1)` proved every tuple has a
                // first column.
                let id = &tuple[0];
                if !counter.contains_key(id) {
                    counter.insert(id.clone(), (0, 0, 0));
                }
                cancel.check()?;
            }
        }
        for (k, (total_d, out_d, in_d)) in counter.into_iter() {
            let tuple = vec![
                k,
                DataValue::from(total_d as i64),
                DataValue::from(out_d as i64),
                DataValue::from(in_d as i64),
            ];
            out.put(Tuple::from_vec(tuple))?;
        }
        Ok(())
    }

    fn arity(
        &self,
        _options: &BTreeMap<SmartString<LazyCompact>, Expr>,
        _rule_head: &[Symbol],
        _span: SourceSpan,
    ) -> Result<usize> {
        Ok(4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::value::Tuple;
    use crate::fixed_rule::tests_support::{TestInput, run_fixed_rule};

    fn s(v: &str) -> DataValue {
        DataValue::from(v)
    }

    /// VALUE ORACLE: exact (total, out, in) per node on a→b, a→c, b→c,
    /// with an isolated node d contributed by the optional nodes relation.
    ///
    /// Hand count: a: out {b,c} = 2, in 0, total 2
    ///             b: out {c} = 1, in {a} = 1, total 2
    ///             c: out 0, in {a,b} = 2, total 2
    ///             d: touches no edge ⇒ (0,0,0)
    #[test]
    fn exact_degrees_with_isolated_node() {
        let i = |v: i64| DataValue::from(v);
        let got = run_fixed_rule(
            &DegreeCentrality,
            vec![
                TestInput::new(
                    vec!["fr", "to"],
                    vec![
                        vec![s("a"), s("b")],
                        vec![s("a"), s("c")],
                        vec![s("b"), s("c")],
                    ],
                ),
                TestInput::new(
                    vec!["id"],
                    vec![vec![s("a")], vec![s("b")], vec![s("c")], vec![s("d")]],
                ),
            ],
            BTreeMap::new(),
            CancelFlag::default(),
        )
        .unwrap();
        let want: Vec<Tuple> = vec![
            vec![s("a"), i(2), i(2), i(0)],
            vec![s("b"), i(2), i(1), i(1)],
            vec![s("c"), i(2), i(0), i(2)],
            vec![s("d"), i(0), i(0), i(0)],
        ];
        assert_eq!(got, want);
    }
}
