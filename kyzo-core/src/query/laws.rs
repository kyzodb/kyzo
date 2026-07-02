/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The reference semantics of stratified Datalog, as executable law.
//!
//! Everything here is deliberately naive: no indexes, no deltas, no rewrites
//! — just the textbook fixpoint, written to be *obviously* correct. The real
//! engine's optimized evaluation must produce byte-identical answer sets to
//! this oracle on every program the differential tests generate. The oracle
//! is judge, never production code (`cfg(test)` only).
//!
//! The abstract program form is minimal on purpose — relation symbols,
//! variables, `DataValue` constants, optional negation — so it can outlive
//! any concrete AST the engine uses.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use crate::data::tuple::Tuple;
use crate::data::value::DataValue;

pub(crate) type Rel = &'static str;

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Term {
    Var(&'static str),
    Const(DataValue),
}

#[derive(Clone, Debug)]
pub(crate) struct Literal {
    pub rel: Rel,
    pub args: Vec<Term>,
    pub negated: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct Rule {
    pub head_rel: Rel,
    pub head_args: Vec<Term>,
    pub body: Vec<Literal>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Program {
    pub rules: Vec<Rule>,
    pub facts: BTreeMap<Rel, BTreeSet<Tuple>>,
}

/// Why a program is refused. The real compiler must refuse the same
/// programs, for the same reasons.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Rejection {
    /// A head variable is not bound by any positive body literal, or a
    /// negated literal uses a variable no positive literal binds.
    Unsafe(&'static str),
    /// Negation occurs inside a recursive cycle.
    Unstratifiable(&'static str),
}

fn literal_vars(l: &Literal) -> HashSet<&'static str> {
    l.args
        .iter()
        .filter_map(|t| match t {
            Term::Var(v) => Some(*v),
            Term::Const(_) => None,
        })
        .collect()
}

/// Law 4 (rule safety), reference form.
pub(crate) fn check_safety(program: &Program) -> Result<(), Rejection> {
    for rule in &program.rules {
        let positive_vars: HashSet<&str> = rule
            .body
            .iter()
            .filter(|l| !l.negated)
            .flat_map(literal_vars)
            .collect();
        for t in &rule.head_args {
            if let Term::Var(v) = t
                && !positive_vars.contains(v)
            {
                return Err(Rejection::Unsafe(rule.head_rel));
            }
        }
        for l in rule.body.iter().filter(|l| l.negated) {
            if !literal_vars(l).is_subset(&positive_vars) {
                return Err(Rejection::Unsafe(rule.head_rel));
            }
        }
    }
    Ok(())
}

/// Law 2 (stratification), reference form: a program is unstratifiable iff
/// some dependency cycle contains a negative edge.
pub(crate) fn check_stratifiable(program: &Program) -> Result<(), Rejection> {
    // Dependency edges: head -> body relation, tagged by polarity.
    let mut edges: HashMap<Rel, HashSet<Rel>> = HashMap::new();
    let mut neg_edges: Vec<(Rel, Rel)> = Vec::new();
    for rule in &program.rules {
        for l in &rule.body {
            edges.entry(rule.head_rel).or_default().insert(l.rel);
            if l.negated {
                neg_edges.push((rule.head_rel, l.rel));
            }
        }
    }
    let reaches = |from: Rel, to: Rel| -> bool {
        let mut seen = HashSet::new();
        let mut stack = vec![from];
        while let Some(r) = stack.pop() {
            if r == to {
                return true;
            }
            if seen.insert(r) {
                stack.extend(edges.get(r).into_iter().flatten().copied());
            }
        }
        false
    };
    for (head, negated_dep) in &neg_edges {
        if reaches(*negated_dep, *head) {
            return Err(Rejection::Unstratifiable(head));
        }
    }
    Ok(())
}

/// Assign strata: a relation sits strictly above everything it depends on
/// through negation, and at least as high as its positive dependencies.
/// Assumes `check_stratifiable` passed.
fn strata(program: &Program) -> HashMap<Rel, usize> {
    let mut s: HashMap<Rel, usize> = HashMap::new();
    let rels: HashSet<Rel> = program
        .rules
        .iter()
        .flat_map(|r| std::iter::once(r.head_rel).chain(r.body.iter().map(|l| l.rel)))
        .chain(program.facts.keys().copied())
        .collect();
    for r in &rels {
        s.insert(r, 0);
    }
    let bound = rels.len() + 1;
    for _ in 0..bound {
        let mut changed = false;
        for rule in &program.rules {
            let mut need = 0usize;
            for l in &rule.body {
                let dep = s[l.rel] + usize::from(l.negated);
                need = need.max(dep);
            }
            if s[rule.head_rel] < need {
                s.insert(rule.head_rel, need);
                changed = true;
            }
        }
        if !changed {
            return s;
        }
    }
    unreachable!("stratum assignment must converge on stratifiable programs");
}

type Bindings = HashMap<&'static str, DataValue>;

fn unify(args: &[Term], tuple: &Tuple, bound: &Bindings) -> Option<Bindings> {
    if args.len() != tuple.len() {
        return None;
    }
    let mut out = bound.clone();
    for (t, v) in args.iter().zip(tuple) {
        match t {
            Term::Const(c) => {
                if c != v {
                    return None;
                }
            }
            Term::Var(name) => match out.get(name) {
                Some(existing) if existing != v => return None,
                Some(_) => {}
                None => {
                    out.insert(name, v.clone());
                }
            },
        }
    }
    Some(out)
}

fn ground(args: &[Term], bound: &Bindings) -> Tuple {
    args.iter()
        .map(|t| match t {
            Term::Const(c) => c.clone(),
            Term::Var(v) => bound[v].clone(),
        })
        .collect()
}

/// Naive stratified fixpoint evaluation: the textbook algorithm, and the
/// oracle for Laws 1 and 3. Validates safety and stratifiability first.
pub(crate) fn naive_eval(program: &Program) -> Result<BTreeMap<Rel, BTreeSet<Tuple>>, Rejection> {
    check_safety(program)?;
    check_stratifiable(program)?;
    let strata_of = strata(program);
    let max_stratum = strata_of.values().copied().max().unwrap_or(0);

    let mut db = program.facts.clone();
    let empty = BTreeSet::new();

    for stratum in 0..=max_stratum {
        // Law 3's embodiment: over finite data with no invented values the
        // fixpoint is reached in finitely many rounds; the generous bound
        // turns non-termination into a loud test failure.
        let mut rounds = 0usize;
        loop {
            rounds += 1;
            assert!(
                rounds <= 100_000,
                "fixpoint bound exceeded: non-termination"
            );
            let mut changed = false;
            for rule in program
                .rules
                .iter()
                .filter(|r| strata_of[r.head_rel] == stratum)
            {
                // Positives first (safety guarantees negated vars are then bound).
                let mut ordered: Vec<&Literal> = rule.body.iter().filter(|l| !l.negated).collect();
                ordered.extend(rule.body.iter().filter(|l| l.negated));

                let mut frontier: Vec<Bindings> = vec![Bindings::new()];
                for lit in ordered {
                    let mut next = Vec::new();
                    for bound in &frontier {
                        if lit.negated {
                            let probe = ground(&lit.args, bound);
                            if !db.get(lit.rel).unwrap_or(&empty).contains(&probe) {
                                next.push(bound.clone());
                            }
                        } else {
                            for tuple in db.get(lit.rel).unwrap_or(&empty) {
                                if let Some(b) = unify(&lit.args, tuple, bound) {
                                    next.push(b);
                                }
                            }
                        }
                    }
                    frontier = next;
                }
                for bound in frontier {
                    let derived = ground(&rule.head_args, &bound);
                    if db.entry(rule.head_rel).or_default().insert(derived) {
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }
    }
    Ok(db)
}

/// The corpus of programs the compiler must refuse — shared between the
/// reference checker's self-tests and (as they land) the real compiler's.
pub(crate) fn unstratifiable_corpus() -> Vec<(&'static str, Program)> {
    fn lit(rel: Rel, args: Vec<Term>, negated: bool) -> Literal {
        Literal { rel, args, negated }
    }
    let x = || Term::Var("X");
    let y = || Term::Var("Y");
    vec![
        (
            "direct self-negation: p(X) :- d(X), not p(X)",
            Program {
                rules: vec![Rule {
                    head_rel: "p",
                    head_args: vec![x()],
                    body: vec![lit("d", vec![x()], false), lit("p", vec![x()], true)],
                }],
                facts: Default::default(),
            },
        ),
        (
            "mutual negation: p :- d, not q; q :- d, not p",
            Program {
                rules: vec![
                    Rule {
                        head_rel: "p",
                        head_args: vec![x()],
                        body: vec![lit("d", vec![x()], false), lit("q", vec![x()], true)],
                    },
                    Rule {
                        head_rel: "q",
                        head_args: vec![x()],
                        body: vec![lit("d", vec![x()], false), lit("p", vec![x()], true)],
                    },
                ],
                facts: Default::default(),
            },
        ),
        (
            "win-move game: win(X) :- move(X,Y), not win(Y)",
            Program {
                rules: vec![Rule {
                    head_rel: "win",
                    head_args: vec![x()],
                    body: vec![
                        lit("move", vec![x(), y()], false),
                        lit("win", vec![y()], true),
                    ],
                }],
                facts: Default::default(),
            },
        ),
        (
            "negation through a positive cycle: a :- d, not b; b :- a",
            Program {
                rules: vec![
                    Rule {
                        head_rel: "a",
                        head_args: vec![x()],
                        body: vec![lit("d", vec![x()], false), lit("b", vec![x()], true)],
                    },
                    Rule {
                        head_rel: "b",
                        head_args: vec![x()],
                        body: vec![lit("a", vec![x()], false)],
                    },
                ],
                facts: Default::default(),
            },
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(i: i64) -> DataValue {
        DataValue::from(i)
    }
    fn edge_facts(edges: &[(i64, i64)]) -> BTreeMap<Rel, BTreeSet<Tuple>> {
        let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = Default::default();
        facts.insert(
            "edge",
            edges.iter().map(|(a, b)| vec![v(*a), v(*b)]).collect(),
        );
        facts
    }
    fn lit(rel: Rel, args: Vec<Term>, negated: bool) -> Literal {
        Literal { rel, args, negated }
    }
    fn x() -> Term {
        Term::Var("X")
    }
    fn y() -> Term {
        Term::Var("Y")
    }
    fn z() -> Term {
        Term::Var("Z")
    }

    /// path(X,Y) :- edge(X,Y); path(X,Y) :- edge(X,Z), path(Z,Y).
    fn transitive_closure() -> Vec<Rule> {
        vec![
            Rule {
                head_rel: "path",
                head_args: vec![x(), y()],
                body: vec![lit("edge", vec![x(), y()], false)],
            },
            Rule {
                head_rel: "path",
                head_args: vec![x(), y()],
                body: vec![
                    lit("edge", vec![x(), z()], false),
                    lit("path", vec![z(), y()], false),
                ],
            },
        ]
    }

    #[test]
    fn law1_transitive_closure_exact() {
        let program = Program {
            rules: transitive_closure(),
            facts: edge_facts(&[(1, 2), (2, 3), (3, 4)]),
        };
        let db = naive_eval(&program).unwrap();
        let want: BTreeSet<Tuple> = [(1, 2), (2, 3), (3, 4), (1, 3), (2, 4), (1, 4)]
            .into_iter()
            .map(|(a, b)| vec![v(a), v(b)])
            .collect();
        assert_eq!(db["path"], want);
    }

    #[test]
    fn law3_recursion_terminates_on_cyclic_data() {
        let program = Program {
            rules: transitive_closure(),
            facts: edge_facts(&[(1, 2), (2, 3), (3, 1)]),
        };
        let db = naive_eval(&program).unwrap();
        // Full 3×3 closure on a cycle.
        assert_eq!(db["path"].len(), 9);
    }

    #[test]
    fn law2_stratified_negation_evaluates_correctly() {
        // unreachable(X,Y) :- node(X), node(Y), not path(X,Y).
        let mut facts = edge_facts(&[(1, 2), (2, 3)]);
        facts.insert("node", (1..=3).map(|i| vec![v(i)]).collect());
        let mut rules = transitive_closure();
        rules.push(Rule {
            head_rel: "unreachable",
            head_args: vec![x(), y()],
            body: vec![
                lit("node", vec![x()], false),
                lit("node", vec![y()], false),
                lit("path", vec![x(), y()], true),
            ],
        });
        let db = naive_eval(&Program { rules, facts }).unwrap();
        let want: BTreeSet<Tuple> = [(1, 1), (2, 1), (2, 2), (3, 1), (3, 2), (3, 3)]
            .into_iter()
            .map(|(a, b)| vec![v(a), v(b)])
            .collect();
        assert_eq!(db["unreachable"], want);
    }

    #[test]
    fn law2_unstratifiable_corpus_is_refused() {
        for (name, program) in unstratifiable_corpus() {
            assert!(
                matches!(
                    check_stratifiable(&program),
                    Err(Rejection::Unstratifiable(_))
                ),
                "must refuse: {name}"
            );
            assert!(naive_eval(&program).is_err(), "eval must refuse: {name}");
        }
    }

    #[test]
    fn law4_unsafe_rules_are_refused() {
        // Head variable unbound by any positive literal.
        let unbound_head = Program {
            rules: vec![Rule {
                head_rel: "p",
                head_args: vec![x()],
                body: vec![lit("q", vec![y()], false)],
            }],
            facts: Default::default(),
        };
        assert_eq!(check_safety(&unbound_head), Err(Rejection::Unsafe("p")));

        // Negated literal over a variable no positive literal binds.
        let unbound_negation = Program {
            rules: vec![Rule {
                head_rel: "p",
                head_args: vec![x()],
                body: vec![lit("q", vec![x()], false), lit("r", vec![z()], true)],
            }],
            facts: Default::default(),
        };
        assert_eq!(check_safety(&unbound_negation), Err(Rejection::Unsafe("p")));
    }

    #[test]
    fn constants_and_repeated_variables_unify_exactly() {
        // same(X) :- edge(X, X).  eq3(X) :- edge(3, X).
        let mut facts = edge_facts(&[(1, 1), (1, 2), (3, 5)]);
        facts.get_mut("edge").unwrap().insert(vec![v(4), v(4)]);
        let program = Program {
            rules: vec![
                Rule {
                    head_rel: "same",
                    head_args: vec![x()],
                    body: vec![lit("edge", vec![x(), x()], false)],
                },
                Rule {
                    head_rel: "eq3",
                    head_args: vec![x()],
                    body: vec![lit("edge", vec![Term::Const(v(3)), x()], false)],
                },
            ],
            facts,
        };
        let db = naive_eval(&program).unwrap();
        assert_eq!(db["same"], [vec![v(1)], vec![v(4)]].into_iter().collect());
        assert_eq!(db["eq3"], [vec![v(5)]].into_iter().collect());
    }
}
