/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Analyzer-coupled tokenize extension over the model FTS AST.
//!
//! [`kyzo_model::parse::search`] owns [`FtsExpr`] / [`FtsLiteral`] /
//! [`FtsNear`]. This module rewrites those types through a
//! [`TextAnalyzer`] so a query is matched with exactly the pipeline that
//! built the index. PREFIX literals pass through whole: filtering or
//! stemming a prefix pattern would change what it means.

use kyzo_model::parse::search::{FtsExpr, FtsLiteral, FtsNear};
use smartstring::SmartString;

use crate::project::text::tokenizer::TextAnalyzer;

/// Re-tokenize an FTS literal through the index's analyzer.
pub(crate) trait TokenizeFtsLiteral {
    fn tokenize(self, tokenizer: &TextAnalyzer, coll: &mut Vec<FtsLiteral>);
}

impl TokenizeFtsLiteral for FtsLiteral {
    fn tokenize(self, tokenizer: &TextAnalyzer, coll: &mut Vec<FtsLiteral>) {
        if self.is_prefix() {
            coll.push(self);
            return;
        }

        let mut tokens = tokenizer.token_stream(self.value());
        while let Some(t) = tokens.next() {
            if let Some(l) = FtsLiteral::new(SmartString::from(&t.text), false, self.booster()) {
                coll.push(l);
            }
        }
    }
}

/// Rewrite every literal through the index's analyzer, then flatten.
pub(crate) trait TokenizeFtsExpr {
    fn tokenize(self, tokenizer: &TextAnalyzer) -> FtsExpr;
}

impl TokenizeFtsExpr for FtsExpr {
    fn tokenize(self, tokenizer: &TextAnalyzer) -> FtsExpr {
        self.do_tokenize(tokenizer).flatten()
    }
}

trait DoTokenize {
    fn do_tokenize(self, tokenizer: &TextAnalyzer) -> FtsExpr;
}

impl DoTokenize for FtsExpr {
    fn do_tokenize(self, tokenizer: &TextAnalyzer) -> FtsExpr {
        match self {
            FtsExpr::Literal(l) => {
                let mut tokens = vec![];
                l.tokenize(tokenizer, &mut tokens);
                match tokens.len() {
                    0 => FtsExpr::empty_node(),
                    1 => FtsExpr::Literal(tokens.remove(0)),
                    _ => FtsExpr::and(tokens.into_iter().map(FtsExpr::Literal).collect()),
                }
            }
            FtsExpr::Near(FtsNear { literals, distance }) => {
                let mut tokens = vec![];
                for l in literals.into_vec() {
                    l.tokenize(tokenizer, &mut tokens);
                }
                FtsExpr::near(tokens, distance)
            }
            FtsExpr::And(exprs) => FtsExpr::and(
                exprs
                    .into_vec()
                    .into_iter()
                    .map(|e| e.do_tokenize(tokenizer))
                    .collect(),
            ),
            FtsExpr::Or(exprs) => FtsExpr::or(
                exprs
                    .into_vec()
                    .into_iter()
                    .map(|e| e.do_tokenize(tokenizer))
                    .collect(),
            ),
            FtsExpr::Not(lhs, rhs) => FtsExpr::Not(
                Box::new(lhs.do_tokenize(tokenizer)),
                Box::new(rhs.do_tokenize(tokenizer)),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DataValue;
    use crate::project::text::TokenizerConfig;

    fn lit(s: &str) -> FtsExpr {
        if s.is_empty() {
            FtsExpr::empty_node()
        } else {
            FtsExpr::Literal(FtsLiteral::new(s.into(), false, 1.0).unwrap())
        }
    }

    fn analyzer(tk: &str, filters: &[(&str, Vec<DataValue>)]) -> TextAnalyzer {
        let tk = TokenizerConfig::admit(tk, vec![]).expect("test tokenizer");
        let filters: Vec<_> = filters
            .iter()
            .map(|(n, args)| TokenizerConfig::admit(*n, args.clone()).expect("test filter"))
            .collect();
        tk.build(&filters).unwrap()
    }

    #[test]
    fn tokenize_rewrites_literals_through_the_analyzer() {
        let an = analyzer(
            "Simple",
            &[
                ("Lowercase", vec![]),
                ("Stemmer", vec![DataValue::from("english")]),
            ],
        );
        let e = lit("Running Dogs").tokenize(&an);
        match &e {
            FtsExpr::And(v) => {
                assert_eq!(v.len(), 2);
                assert_eq!(v[0], lit("run"));
                assert_eq!(v[1], lit("dog"));
            }
            other @ FtsExpr::Literal(_)
            | other @ FtsExpr::Near(_)
            | other @ FtsExpr::Or(_)
            | other @ FtsExpr::Not(..) => panic!("expected And, got {other:?}"),
        }
        assert_eq!(lit("Running").tokenize(&an), lit("run"));
        let stop = analyzer("Simple", &[("Stopwords", vec![DataValue::from("en")])]);
        let e = FtsExpr::and(vec![lit("the"), lit("crafty fox")]).tokenize(&stop);
        match &e {
            FtsExpr::And(v) => {
                // Content pin: "the" vanishes; "crafty fox" survives as two
                // literals — length alone would green on ["x","y"].
                assert_eq!(
                    v.as_slice(),
                    &[lit("crafty"), lit("fox")],
                    "stopword must drop 'the' and keep crafty/fox: {v:?}"
                );
            }
            other @ FtsExpr::Literal(_)
            | other @ FtsExpr::Near(_)
            | other @ FtsExpr::Or(_)
            | other @ FtsExpr::Not(..) => panic!("expected And, got {other:?}"),
        }
        let p = FtsExpr::Literal(FtsLiteral::new("Runni".into(), true, 2.0).unwrap());
        assert_eq!(p.clone().tokenize(&an), p);
        let e = FtsExpr::near(
            vec![
                FtsLiteral::new("Running".into(), false, 1.0).unwrap(),
                FtsLiteral::new("Dogs".into(), false, 1.0).unwrap(),
            ],
            3,
        )
        .tokenize(&an);
        match e {
            FtsExpr::Near(FtsNear { literals, distance }) => {
                assert_eq!(distance, 3);
                assert_eq!(literals.len(), 2);
                assert_eq!(literals.as_slice()[0].value(), "run");
                assert_eq!(literals.as_slice()[1].value(), "dog");
            }
            other @ FtsExpr::Literal(_)
            | other @ FtsExpr::And(_)
            | other @ FtsExpr::Or(_)
            | other @ FtsExpr::Not(..) => panic!("expected Near, got {other:?}"),
        }
    }
}
