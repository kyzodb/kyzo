/*
 * Copyright 2023, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0): this is the permanent home of the FTS query AST that
 * `parse/fts.rs` declared as a seam before this tier landed; the
 * single-element collapses in `flatten` and `do_tokenize` use bounds-checked
 * `remove(0)` instead of `into_iter().next().unwrap()` (no unwraps on the
 * user-text path, per the engine's law).
 */

//! The FTS query AST: what an FTS search string *means* once parsed.
//!
//! [`FtsExpr`] is produced by `crate::parse::fts::parse_fts_query` from the
//! query mini-language (`AND`/`OR`/`NOT`, `NEAR(...)`, quoting, `*` prefix
//! markers, `^` boosters). It is pure data plus three total rewrites:
//! [`FtsExpr::flatten`], [`FtsExpr::is_empty`], and [`FtsExpr::tokenize`] —
//! the last one rewrites every literal through the index's own
//! [`TextAnalyzer`] so a query is matched with exactly the pipeline that
//! built the index.

use crate::engines::text::tokenizer::TextAnalyzer;
use ordered_float::OrderedFloat;
use smartstring::{LazyCompact, SmartString};

/// One search term: the text, whether it is a prefix search, and its
/// score booster.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct FtsLiteral {
    pub(crate) value: SmartString<LazyCompact>,
    pub(crate) is_prefix: bool,
    pub(crate) booster: OrderedFloat<f64>,
}

impl FtsLiteral {
    /// Re-tokenize this literal through the index's analyzer, pushing the
    /// resulting terms into `coll`. Prefix literals pass through whole: a
    /// prefix search matches stored terms by byte prefix, so filtering or
    /// stemming the pattern would change what it means.
    pub(crate) fn tokenize(self, tokenizer: &TextAnalyzer, coll: &mut Vec<Self>) {
        if self.is_prefix {
            coll.push(self);
            return;
        }

        let mut tokens = tokenizer.token_stream(&self.value);
        while let Some(t) = tokens.next() {
            coll.push(FtsLiteral {
                value: SmartString::from(&t.text),
                is_prefix: false,
                booster: self.booster,
            })
        }
    }
}

/// A proximity group: literals that must occur within `distance` tokens.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct FtsNear {
    pub(crate) literals: Vec<FtsLiteral>,
    pub(crate) distance: u32,
}

/// A parsed FTS query.
///
/// # Depth invariant (load-bearing)
///
/// `crate::parse::fts::parse_fts_query` is the only non-test constructor,
/// and it bounds construction: group and `NOT` depth are counted against
/// `crate::parse::NESTING_CEILING`, the total operator count against
/// `crate::parse::fts::FTS_OPS_CEILING`, and `AND`/`OR` chains are built
/// as flat vectors — so **no `FtsExpr` deeper than `NESTING_CEILING` plus
/// a small constant ever exists**. Every recursive walk here (`flatten`,
/// `is_empty`, `do_tokenize`) and the compiler-generated recursive
/// `Drop`/`Clone`/`PartialEq`/`Hash` rely on that bound for stack safety;
/// they are recursive *because* the bound holds. (Bounding at the parser
/// is strictly stronger than rewriting `flatten` iteratively: an
/// iterative `flatten` would still leave the derived `Drop` and friends
/// recursing over an unbounded tree.) A new constructor must either
/// enforce the same bound or make every walk, including `Drop`,
/// iterative.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum FtsExpr {
    Literal(FtsLiteral),
    Near(FtsNear),
    And(Vec<FtsExpr>),
    Or(Vec<FtsExpr>),
    Not(Box<FtsExpr>, Box<FtsExpr>),
}

impl FtsExpr {
    /// Rewrite every literal through the index's analyzer, then
    /// [`flatten`](Self::flatten). A literal that tokenizes to several terms
    /// becomes a conjunction of them; one that tokenizes to nothing (all
    /// stopwords, say) becomes an empty node that `flatten` drops.
    pub(crate) fn tokenize(self, tokenizer: &TextAnalyzer) -> Self {
        self.do_tokenize(tokenizer).flatten()
    }

    pub(crate) fn is_empty(&self) -> bool {
        match self {
            FtsExpr::Literal(l) => l.booster == 0. || l.value.is_empty(),
            FtsExpr::Near(FtsNear { literals, .. }) => literals.is_empty(),
            FtsExpr::And(v) => v.is_empty(),
            FtsExpr::Or(v) => v.is_empty(),
            FtsExpr::Not(lhs, _) => lhs.is_empty(),
        }
    }

    /// Collapse nested conjunctions/disjunctions and drop empty subtrees.
    pub(crate) fn flatten(self) -> Self {
        match self {
            FtsExpr::And(exprs) => {
                let mut flattened = vec![];
                for e in exprs {
                    match e.flatten() {
                        FtsExpr::And(es) => flattened.extend(es),
                        e @ FtsExpr::Literal(_) | e @ FtsExpr::Near(_) | e @ FtsExpr::Or(_) | e @ FtsExpr::Not(..) => {
                            if !e.is_empty() {
                                flattened.push(e)
                            }
                        }
                    }
                }
                if flattened.len() == 1 {
                    // In bounds: length checked to be exactly 1.
                    flattened.remove(0)
                } else {
                    FtsExpr::And(flattened)
                }
            }
            FtsExpr::Or(exprs) => {
                let mut flattened = vec![];
                for e in exprs {
                    match e.flatten() {
                        FtsExpr::Or(es) => flattened.extend(es),
                        e @ FtsExpr::Literal(_) | e @ FtsExpr::Near(_) | e @ FtsExpr::And(_) | e @ FtsExpr::Not(..) => {
                            if !e.is_empty() {
                                flattened.push(e)
                            }
                        }
                    }
                }
                if flattened.len() == 1 {
                    // In bounds: length checked to be exactly 1.
                    flattened.remove(0)
                } else {
                    FtsExpr::Or(flattened)
                }
            }
            FtsExpr::Not(lhs, rhs) => {
                let lhs = lhs.flatten();
                let rhs = rhs.flatten();
                if rhs.is_empty() {
                    lhs
                } else {
                    FtsExpr::Not(Box::new(lhs), Box::new(rhs))
                }
            }
            FtsExpr::Literal(l) => FtsExpr::Literal(l),
            FtsExpr::Near(n) => FtsExpr::Near(n),
        }
    }

    fn do_tokenize(self, tokenizer: &TextAnalyzer) -> Self {
        match self {
            FtsExpr::Literal(l) => {
                let mut tokens = vec![];
                l.tokenize(tokenizer, &mut tokens);
                if tokens.len() == 1 {
                    // In bounds: length checked to be exactly 1.
                    FtsExpr::Literal(tokens.remove(0))
                } else {
                    FtsExpr::And(tokens.into_iter().map(FtsExpr::Literal).collect())
                }
            }
            FtsExpr::Near(FtsNear { literals, distance }) => {
                let mut tokens = vec![];
                for l in literals {
                    l.tokenize(tokenizer, &mut tokens);
                }
                FtsExpr::Near(FtsNear {
                    literals: tokens,
                    distance,
                })
            }
            FtsExpr::And(exprs) => FtsExpr::And(
                exprs
                    .into_iter()
                    .map(|e| e.do_tokenize(tokenizer))
                    .collect(),
            ),
            FtsExpr::Or(exprs) => FtsExpr::Or(
                exprs
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
    use crate::engines::text::TokenizerConfig;

    fn lit(s: &str) -> FtsExpr {
        FtsExpr::Literal(FtsLiteral {
            value: s.into(),
            is_prefix: false,
            booster: 1.0.into(),
        })
    }

    fn analyzer(tk: &str, filters: &[(&str, Vec<DataValue>)]) -> TextAnalyzer {
        let tk = TokenizerConfig {
            name: tk.into(),
            args: vec![],
        };
        let filters: Vec<_> = filters
            .iter()
            .map(|(n, args)| TokenizerConfig {
                name: (*n).into(),
                args: args.clone(),
            })
            .collect();
        tk.build(&filters).unwrap()
    }

    #[test]
    fn is_empty_edge_cases() {
        assert!(lit("").is_empty());
        // A zero booster empties a literal even with text.
        assert!(
            FtsExpr::Literal(FtsLiteral {
                value: "hello".into(),
                is_prefix: false,
                booster: 0.0.into(),
            })
            .is_empty()
        );
        assert!(FtsExpr::And(vec![]).is_empty());
        assert!(FtsExpr::Or(vec![]).is_empty());
        assert!(
            FtsExpr::Near(FtsNear {
                literals: vec![],
                distance: 10
            })
            .is_empty()
        );
        // Not is empty iff its keep-side is empty.
        assert!(FtsExpr::Not(Box::new(lit("")), Box::new(lit("x"))).is_empty());
        assert!(!FtsExpr::Not(Box::new(lit("x")), Box::new(lit(""))).is_empty());
        // But And/Or containing only empties are NOT empty until flattened:
        // is_empty is shallow by design; flatten is the normalizer.
        let shallow = FtsExpr::And(vec![lit("")]);
        assert!(!shallow.is_empty());
        assert!(shallow.flatten().is_empty());
    }

    #[test]
    fn flatten_collapses_nesting_and_drops_empties() {
        // And(And(a,b), c) → And(a,b,c)
        let e = FtsExpr::And(vec![FtsExpr::And(vec![lit("a"), lit("b")]), lit("c")]);
        match e.flatten() {
            FtsExpr::And(v) => assert_eq!(v.len(), 3),
            other @ FtsExpr::Literal(_) | other @ FtsExpr::Near(_) | other @ FtsExpr::Or(_) | other @ FtsExpr::Not(..) => panic!("expected And, got {other:?}"),
        }
        // Or(Or(a,b), Or(c,d)) → Or(a,b,c,d)
        let e = FtsExpr::Or(vec![
            FtsExpr::Or(vec![lit("a"), lit("b")]),
            FtsExpr::Or(vec![lit("c"), lit("d")]),
        ]);
        match e.flatten() {
            FtsExpr::Or(v) => assert_eq!(v.len(), 4),
            other @ FtsExpr::Literal(_) | other @ FtsExpr::Near(_) | other @ FtsExpr::And(_) | other @ FtsExpr::Not(..) => panic!("expected Or, got {other:?}"),
        }
        // Single-survivor collapse: And(a, "") → a
        let e = FtsExpr::And(vec![lit("a"), lit("")]);
        assert_eq!(e.flatten(), lit("a"));
        // All-empty collapse: Or("", "") → Or([]) (empty, not a panic)
        let e = FtsExpr::Or(vec![lit(""), lit("")]);
        assert!(e.flatten().is_empty());
        // Not with empty rhs collapses to lhs.
        let e = FtsExpr::Not(Box::new(lit("keep")), Box::new(lit("")));
        assert_eq!(e.flatten(), lit("keep"));
        // Deep mixed nesting terminates and normalizes.
        let e = FtsExpr::And(vec![FtsExpr::And(vec![FtsExpr::And(vec![FtsExpr::Or(
            vec![lit("x")],
        )])])]);
        assert_eq!(e.flatten(), lit("x"));
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
        // Multi-word literal becomes a conjunction of stemmed terms.
        let e = lit("Running Dogs").tokenize(&an);
        match &e {
            FtsExpr::And(v) => {
                assert_eq!(v.len(), 2);
                assert_eq!(v[0], lit("run"));
                assert_eq!(v[1], lit("dog"));
            }
            other @ FtsExpr::Literal(_) | other @ FtsExpr::Near(_) | other @ FtsExpr::Or(_) | other @ FtsExpr::Not(..) => panic!("expected And, got {other:?}"),
        }
        // A literal that tokenizes to one term stays a literal.
        assert_eq!(lit("Running").tokenize(&an), lit("run"));
        // A literal that tokenizes to nothing flattens away inside And.
        let stop = analyzer("Simple", &[("Stopwords", vec![DataValue::from("en")])]);
        let e = FtsExpr::And(vec![lit("the"), lit("crafty fox")]).tokenize(&stop);
        match &e {
            FtsExpr::And(v) => assert_eq!(v.len(), 2, "'the' must vanish: {v:?}"),
            other @ FtsExpr::Literal(_) | other @ FtsExpr::Near(_) | other @ FtsExpr::Or(_) | other @ FtsExpr::Not(..) => panic!("expected And, got {other:?}"),
        }
        // Prefix literals pass through untouched.
        let p = FtsExpr::Literal(FtsLiteral {
            value: "Runni".into(),
            is_prefix: true,
            booster: 2.0.into(),
        });
        assert_eq!(p.clone().tokenize(&an), p);
        // Near re-tokenizes its members but keeps the distance.
        let e = FtsExpr::Near(FtsNear {
            literals: vec![
                FtsLiteral {
                    value: "Running".into(),
                    is_prefix: false,
                    booster: 1.0.into(),
                },
                FtsLiteral {
                    value: "Dogs".into(),
                    is_prefix: false,
                    booster: 1.0.into(),
                },
            ],
            distance: 3,
        })
        .tokenize(&an);
        match e {
            FtsExpr::Near(FtsNear { literals, distance }) => {
                assert_eq!(distance, 3);
                assert_eq!(literals.len(), 2);
                assert_eq!(literals[0].value, "run");
                assert_eq!(literals[1].value, "dog");
            }
            other @ FtsExpr::Literal(_) | other @ FtsExpr::And(_) | other @ FtsExpr::Or(_) | other @ FtsExpr::Not(..) => panic!("expected Near, got {other:?}"),
        }
    }
}
