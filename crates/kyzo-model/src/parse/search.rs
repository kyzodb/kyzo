/*
 * Copyright 2023, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0): permanent home of the FTS query AST pure-data half. Analyzer-
 * coupled tokenize lives in kyzo-core `project/text/ast.rs` as an extension
 * over these types (crate wall: kyzo-model cannot depend on the engine).
 */

//! The FTS query AST: what an FTS search string *means* once parsed.
//!
//! Pure data plus total rewrites [`FtsExpr::flatten`] and
//! [`FtsExpr::is_empty`]. Analyzer-coupled tokenize is an extension in the
//! engine crate over these types.
//!
//! # Depth invariant (load-bearing)
//!
//! The FTS query parser is the only non-test constructor, and it bounds
//! construction: group and `NOT` depth are counted against a nesting
//! ceiling, the total operator count against an ops ceiling, and
//! `AND`/`OR` chains are built as flat vectors — so **no `FtsExpr` deeper
//! than that nesting ceiling plus a small constant ever exists**. Every
//! recursive walk here (`flatten`, `is_empty`) and the compiler-generated
//! recursive `Drop`/`Clone`/`PartialEq`/`Hash` rely on that bound for
//! stack safety; they are recursive *because* the bound holds. (Bounding
//! at the parser is strictly stronger than rewriting `flatten`
//! iteratively: an iterative `flatten` would still leave the derived
//! `Drop` and friends recursing over an unbounded tree.) A new
//! constructor must either enforce the same bound or make every walk,
//! including `Drop`, iterative.

use std::hash::{Hash, Hasher};
use std::ops::Index;

use smartstring::{LazyCompact, SmartString};

/// Score booster for one FTS literal (`^n`). Bit-identity Eq/Hash so NaN
/// boosters are representable without a float-order crate at the model wall.
#[derive(Debug, Clone, Copy)]
pub struct FtsBooster(pub f64);

impl PartialEq for FtsBooster {
    fn eq(&self, other: &Self) -> bool {
        self.0.to_bits() == other.0.to_bits()
    }
}

impl Eq for FtsBooster {}

impl Hash for FtsBooster {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.to_bits().hash(state);
    }
}

impl From<f64> for FtsBooster {
    fn from(v: f64) -> Self {
        FtsBooster(v)
    }
}

impl From<f32> for FtsBooster {
    fn from(v: f32) -> Self {
        FtsBooster(f64::from(v))
    }
}

/// One search term: the text, whether it is a prefix search, and its
/// score booster.
///
/// Mint only through [`Self::new`]. Empty text is the canonical empty
/// node (`is_prefix == false`, `booster == 0`); searchable terms require
/// non-empty text and a finite positive booster.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FtsLiteral {
    value: SmartString<LazyCompact>,
    is_prefix: bool,
    booster: FtsBooster,
}

impl FtsLiteral {
    /// Sole mint. Empty value is only the canonical empty node; non-empty
    /// values require a finite positive booster.
    pub fn new(
        value: SmartString<LazyCompact>,
        is_prefix: bool,
        booster: impl Into<FtsBooster>,
    ) -> Option<Self> {
        let booster = booster.into();
        if value.is_empty() {
            if is_prefix || booster != FtsBooster(0.0) {
                return None;
            }
        } else if !(booster.0.is_finite() && booster.0 > 0.0) {
            return None;
        }
        Some(FtsLiteral {
            value,
            is_prefix,
            booster,
        })
    }

    pub fn value(&self) -> &str {
        self.value.as_str()
    }

    pub fn is_prefix(&self) -> bool {
        self.is_prefix
    }

    pub fn booster(&self) -> FtsBooster {
        self.booster
    }
}

/// Non-empty Near literals. Empty proximity is unrepresentable through
/// [`Self::admit`] / [`FtsExpr::near`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NonEmptyFtsLiterals {
    literals: Vec<FtsLiteral>,
}

impl NonEmptyFtsLiterals {
    pub fn admit(literals: Vec<FtsLiteral>) -> Option<Self> {
        if literals.is_empty() {
            None
        } else {
            Some(Self { literals })
        }
    }

    pub fn as_slice(&self) -> &[FtsLiteral] {
        &self.literals
    }

    pub fn len(&self) -> usize {
        self.literals.len()
    }

    pub fn into_vec(self) -> Vec<FtsLiteral> {
        self.literals
    }
}

/// A proximity group: literals that must occur within `distance` tokens.
/// Literals are non-empty by construction.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FtsNear {
    pub literals: NonEmptyFtsLiterals,
    pub distance: u32,
}

/// Non-empty And/Or children. Empty conjunction/disjunction is
/// unrepresentable through [`Self::admit`] / [`FtsExpr::and`] /
/// [`FtsExpr::or`]; [`FtsExpr::flatten`] never emits empty And/Or either
/// (collapses to [`FtsExpr::empty_node`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NonEmptyFtsExprs {
    children: Vec<FtsExpr>,
}

impl NonEmptyFtsExprs {
    pub fn admit(children: Vec<FtsExpr>) -> Option<Self> {
        if children.is_empty() {
            None
        } else {
            Some(Self { children })
        }
    }

    pub fn into_vec(self) -> Vec<FtsExpr> {
        self.children
    }

    pub fn as_slice(&self) -> &[FtsExpr] {
        &self.children
    }

    pub fn len(&self) -> usize {
        self.children.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = &FtsExpr> {
        self.children.iter()
    }

    pub fn push(&mut self, e: FtsExpr) {
        self.children.push(e);
    }
}

impl Index<usize> for NonEmptyFtsExprs {
    type Output = FtsExpr;

    fn index(&self, index: usize) -> &Self::Output {
        &self.children[index]
    }
}

/// A parsed FTS query. See the module-level depth invariant.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FtsExpr {
    Literal(FtsLiteral),
    Near(FtsNear),
    And(NonEmptyFtsExprs),
    Or(NonEmptyFtsExprs),
    Not(Box<FtsExpr>, Box<FtsExpr>),
}

impl FtsExpr {
    /// Canonical empty node (empty literal). Used when And/Or would otherwise
    /// be empty after flatten/tokenize — empty And/Or is refused.
    pub fn empty_node() -> Self {
        FtsExpr::Literal(
            FtsLiteral::new(SmartString::new(), false, FtsBooster(0.0))
                .expect("canonical empty literal"),
        )
    }

    /// Conjunction door: refuses empty children (returns [`Self::empty_node`]).
    pub fn and(children: Vec<FtsExpr>) -> Self {
        match NonEmptyFtsExprs::admit(children) {
            Some(n) => FtsExpr::And(n),
            None => Self::empty_node(),
        }
    }

    /// Disjunction door: refuses empty children (returns [`Self::empty_node`]).
    pub fn or(children: Vec<FtsExpr>) -> Self {
        match NonEmptyFtsExprs::admit(children) {
            Some(n) => FtsExpr::Or(n),
            None => Self::empty_node(),
        }
    }

    /// Proximity door: refuses empty literals (returns [`Self::empty_node`]).
    pub fn near(literals: Vec<FtsLiteral>, distance: u32) -> Self {
        match NonEmptyFtsLiterals::admit(literals) {
            Some(l) => FtsExpr::Near(FtsNear {
                literals: l,
                distance,
            }),
            None => Self::empty_node(),
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            FtsExpr::Literal(l) => l.value().is_empty(),
            FtsExpr::Near(_) => false,
            // NonEmptyFtsExprs is never empty by construction.
            FtsExpr::And(_) | FtsExpr::Or(_) => false,
            FtsExpr::Not(lhs, _) => lhs.is_empty(),
        }
    }

    /// Collapse nested conjunctions/disjunctions and drop empty subtrees.
    /// Never emits empty And/Or — all-empty collapses to [`Self::empty_node`].
    pub fn flatten(self) -> Self {
        match self {
            FtsExpr::And(exprs) => {
                let mut flattened = vec![];
                for e in exprs.into_vec() {
                    match e.flatten() {
                        FtsExpr::And(es) => flattened.extend(es.into_vec()),
                        e @ FtsExpr::Literal(_)
                        | e @ FtsExpr::Near(_)
                        | e @ FtsExpr::Or(_)
                        | e @ FtsExpr::Not(..) => {
                            if !e.is_empty() {
                                flattened.push(e)
                            }
                        }
                    }
                }
                match flattened.len() {
                    0 => Self::empty_node(),
                    1 => flattened.remove(0),
                    _ => FtsExpr::and(flattened),
                }
            }
            FtsExpr::Or(exprs) => {
                let mut flattened = vec![];
                for e in exprs.into_vec() {
                    match e.flatten() {
                        FtsExpr::Or(es) => flattened.extend(es.into_vec()),
                        e @ FtsExpr::Literal(_)
                        | e @ FtsExpr::Near(_)
                        | e @ FtsExpr::And(_)
                        | e @ FtsExpr::Not(..) => {
                            if !e.is_empty() {
                                flattened.push(e)
                            }
                        }
                    }
                }
                match flattened.len() {
                    0 => Self::empty_node(),
                    1 => flattened.remove(0),
                    _ => FtsExpr::or(flattened),
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lit(s: &str) -> FtsExpr {
        if s.is_empty() {
            FtsExpr::empty_node()
        } else {
            FtsExpr::Literal(FtsLiteral::new(s.into(), false, 1.0).unwrap())
        }
    }

    #[test]
    fn is_empty_edge_cases() {
        assert!(lit("").is_empty());
        assert!(FtsLiteral::new("hello".into(), false, 0.0).is_none());
        assert!(FtsExpr::empty_node().is_empty());
        assert!(NonEmptyFtsExprs::admit(vec![]).is_none());
        assert!(FtsExpr::and(vec![]).is_empty());
        assert!(FtsExpr::or(vec![]).is_empty());
        assert!(NonEmptyFtsLiterals::admit(vec![]).is_none());
        assert!(FtsExpr::near(vec![], 10).is_empty());
        assert!(FtsExpr::Not(Box::new(lit("")), Box::new(lit("x"))).is_empty());
        assert!(!FtsExpr::Not(Box::new(lit("x")), Box::new(lit(""))).is_empty());
        let shallow = FtsExpr::and(vec![lit("")]);
        assert!(!shallow.is_empty());
        assert!(shallow.flatten().is_empty());
    }

    #[test]
    fn flatten_collapses_nesting_and_drops_empties() {
        let e = FtsExpr::and(vec![FtsExpr::and(vec![lit("a"), lit("b")]), lit("c")]);
        match e.flatten() {
            FtsExpr::And(v) => assert_eq!(v.len(), 3),
            other @ FtsExpr::Literal(_)
            | other @ FtsExpr::Near(_)
            | other @ FtsExpr::Or(_)
            | other @ FtsExpr::Not(..) => panic!("expected And, got {other:?}"),
        }
        let e = FtsExpr::or(vec![
            FtsExpr::or(vec![lit("a"), lit("b")]),
            FtsExpr::or(vec![lit("c"), lit("d")]),
        ]);
        match e.flatten() {
            FtsExpr::Or(v) => assert_eq!(v.len(), 4),
            other @ FtsExpr::Literal(_)
            | other @ FtsExpr::Near(_)
            | other @ FtsExpr::And(_)
            | other @ FtsExpr::Not(..) => panic!("expected Or, got {other:?}"),
        }
        let e = FtsExpr::and(vec![lit("a"), lit("")]);
        assert_eq!(e.flatten(), lit("a"));
        let e = FtsExpr::or(vec![lit(""), lit("")]);
        let flat = e.flatten();
        assert!(flat.is_empty());
        assert!(matches!(flat, FtsExpr::Literal(_)));
        let e = FtsExpr::Not(Box::new(lit("keep")), Box::new(lit("")));
        assert_eq!(e.flatten(), lit("keep"));
        let e = FtsExpr::and(vec![FtsExpr::and(vec![FtsExpr::and(vec![FtsExpr::or(
            vec![lit("x")],
        )])])]);
        assert_eq!(e.flatten(), lit("x"));
    }
}
