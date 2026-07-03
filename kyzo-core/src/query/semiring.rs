/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Semiring provenance: annotate derived tuples with values from a
//! commutative semiring, so a query's answer can say *why* — with a
//! checkable certificate — instead of only *that*.
//!
//! ## The model (Green–Karvounarakis–Tannen)
//!
//! Every tuple carries an annotation from a commutative semiring
//! `(K, ⊕, ⊗, 0, 1)`. Along one rule body, premise annotations combine
//! with `⊗` (joint use); across alternative derivations of the same tuple
//! they combine with `⊕` (alternative support). Ground facts carry `1`;
//! an underivable tuple stays at `0`.
//!
//! ## What ships here, and the boundary
//!
//! Exactly the two **idempotent** semirings whose fixpoints are finite:
//!
//! - [`Boolean`] — existence. Its support must equal the engine's set
//!   semantics (the evaluator's own fixpoint), proven by differential
//!   against the sealed oracle (`query/provenance.rs`).
//! - [`Tropical`] — min-plus over [`Cost`]: the cheapest derivation, where
//!   a derivation tree's cost is the sum of its rule-application weights
//!   (unit weights make it the number of rule firings). Weights are
//!   [`NonZeroU64`], which is what makes certificate extraction
//!   well-founded (every premise of a min-cost step costs strictly less
//!   than its head).
//!
//! **Counting and polynomial provenance are refused, not approximated**:
//! over recursion they have no finite fixpoint (a cyclic reachability
//! relation has infinitely many derivation trees). They are a different
//! fixpoint with their own annotation store, out of this split's scope —
//! see the capability design's PA3 boundary. Nothing here silently
//! degrades into them: the only implementations of [`Semiring`] in the
//! tree are the two lawful ones, and the solver is armed with a pass
//! ceiling ([`SolverBudget`]) so even a law-breaking implementation
//! refuses (typed) instead of hanging.
//!
//! ## Two-phase evaluation, and why it is sound
//!
//! Annotations are computed **after** the ordinary set-semantics fixpoint,
//! over the grounded derivations the completed stores admit (enumerated by
//! `query/eval.rs::provenance_graph` through the [`RuleBody`] seam). For
//! idempotent semirings this equals the annotated fixpoint: the support of
//! the tropical (or boolean) fixpoint is exactly the set-semantics
//! fixpoint, and the annotations satisfy the same Bellman equations over
//! that support. First-witness recording (the [`WitnessTable`] seam) is
//! *not* enough for tropical — the first derivation found is not the
//! cheapest — which is why the graph enumerates *all* grounded
//! derivations rather than replaying witnesses.
//!
//! Negation and non-meet aggregation are not semiring operations; at those
//! boundaries the annotation collapses to "present" — the tuples of an
//! aggregated or fixed-rule store enter the graph as ground facts
//! (annotation `1`), and full provenance is claimed only for the positive
//! plain-rule fragment above them. Stated, not smuggled.
//!
//! [`RuleBody`]: crate::query::eval::RuleBody
//! [`WitnessTable`]: crate::query::eval::WitnessTable

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use std::num::{NonZeroU32, NonZeroU64};

use miette::{Diagnostic, Result};
use thiserror::Error;

// ─────────────────────────────────────────────────────────────────────────
// Refusals
// ─────────────────────────────────────────────────────────────────────────

/// A tropical `⊗` overflowed `u64`. A real limit, refused typed: costs are
/// exact or absent, never saturated (a silently clamped cost would forge a
/// "cheapest derivation" that does not exist).
#[derive(Debug, Error, Diagnostic, PartialEq, Eq)]
#[error("tropical cost overflow: {left} + {right} exceeds u64")]
#[diagnostic(
    code(provenance::cost_overflow),
    help("lower the rule weights; costs are exact or refused, never saturated")
)]
pub(crate) struct SemiringOverflow {
    pub(crate) left: u64,
    pub(crate) right: u64,
}

/// The provenance pass exceeded an armed ceiling. Both dimensions are
/// deterministic (functions of the graph and the ceiling alone), so the
/// refusal is byte-identical on every run at any thread count.
#[derive(Debug, Error, Diagnostic, PartialEq, Eq)]
#[error("provenance budget exceeded: {dimension} spent {spent} of ceiling {ceiling}")]
#[diagnostic(
    code(provenance::limit_exceeded),
    help("raise the provenance ceiling, or narrow the query")
)]
pub(crate) struct ProvenanceLimitExceeded {
    pub(crate) dimension: &'static str,
    pub(crate) spent: u64,
    pub(crate) ceiling: u64,
}

/// A proof failed verification. The message names the first offending
/// step; a certificate is all-or-nothing.
#[derive(Debug, Error, Diagnostic, PartialEq, Eq)]
#[error("provenance certificate rejected: {0}")]
#[diagnostic(code(provenance::bad_certificate))]
pub(crate) struct BadCertificate(pub(crate) String);

/// Certificate extraction was asked for an underivable tuple (annotation
/// `0` / infinite cost), or for a node the graph does not contain.
#[derive(Debug, Error, Diagnostic, PartialEq, Eq)]
#[error("no derivation to certify: {0}")]
#[diagnostic(code(provenance::no_derivation))]
pub(crate) struct NoDerivation(pub(crate) String);

/// A cross-stage invariant the graph construction should have made
/// impossible (e.g. "a solved min cost is achieved by some edge").
/// Surfaced as an error, never an abort.
#[derive(Debug, Error, Diagnostic)]
#[error("provenance invariant violated: {0}")]
#[diagnostic(code(provenance::invariant), help("This is a bug. Please report it."))]
pub(crate) struct ProvenanceInvariantError(pub(crate) &'static str);

// ─────────────────────────────────────────────────────────────────────────
// The semiring interface
// ─────────────────────────────────────────────────────────────────────────

/// A commutative semiring `(K, ⊕, ⊗, 0, 1)`, as the solver consumes it.
///
/// Laws (asserted on randomized values by the axiom tests in
/// `query/provenance.rs`):
///
/// - `⊕` associative, commutative, identity `0`;
/// - `⊗` associative, commutative, identity `1`, annihilator `0`
///   (`0 ⊗ a = 0`);
/// - `⊗` distributes over `⊕`.
///
/// **Solver contract beyond the axioms**: `⊕` must be idempotent
/// (`a ⊕ a = a`) and every `⊕`-chain `a₀, a₀⊕a₁, …` must stabilize after
/// finitely many strict changes — true of [`Boolean`] (one flip) and
/// [`Tropical`] (a strictly decreasing `u64` chain is finite), and exactly
/// what the counting/polynomial semirings violate over recursion. The
/// solver's pass ceiling turns any violation into a typed refusal rather
/// than divergence.
pub(crate) trait Semiring {
    /// The annotation domain. `Ord` is required for deterministic
    /// rendering and for [`Tropical`]'s `min`; it is not otherwise load
    /// bearing.
    type Value: Clone + Eq + Ord + Debug + Send + Sync;

    /// The additive identity: "no derivation".
    fn zero(&self) -> Self::Value;
    /// The multiplicative identity: "a ground fact".
    fn one(&self) -> Self::Value;
    /// `⊕`: combine alternative derivations. Total (never overflows for
    /// the shipped semirings: `∨` and `min`).
    fn plus(&self, a: &Self::Value, b: &Self::Value) -> Self::Value;
    /// `⊗`: combine jointly-used premises. Fallible: [`Tropical`] refuses
    /// (typed) on `u64` overflow.
    fn times(&self, a: &Self::Value, b: &Self::Value) -> Result<Self::Value, SemiringOverflow>;
    /// Lift one rule application's weight into the semiring: [`Boolean`]
    /// ignores it (`1`), [`Tropical`] charges it.
    fn lift_weight(&self, weight: NonZeroU64) -> Self::Value;
}

/// The boolean semiring `({⊥,⊤}, ∨, ∧, ⊥, ⊤)`: does a derivation exist.
/// Its support is exactly the engine's set semantics — asserted by
/// differential against the oracle, not by this comment.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Boolean;

impl Semiring for Boolean {
    type Value = bool;
    fn zero(&self) -> bool {
        false
    }
    fn one(&self) -> bool {
        true
    }
    fn plus(&self, a: &bool, b: &bool) -> bool {
        *a || *b
    }
    fn times(&self, a: &bool, b: &bool) -> Result<bool, SemiringOverflow> {
        Ok(*a && *b)
    }
    fn lift_weight(&self, _weight: NonZeroU64) -> bool {
        true
    }
}

/// A tropical annotation: the cost of the cheapest known derivation, or
/// [`Cost::Infinite`] for "none". The derived `Ord` is the tropical order
/// (`Finite(a) < Finite(b)` iff `a < b`, every `Finite` below
/// `Infinite`), which makes `⊕ = min` the lattice meet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Cost {
    Finite(u64),
    Infinite,
}

/// The tropical (min-plus) semiring `(ℕ∪{∞}, min, +, ∞, 0)`: the cost of
/// the cheapest derivation, where each rule application charges its
/// weight. With unit weights the cost is the number of rule firings in
/// the derivation tree. (Derivation *depth* would be a min-max algebra,
/// not a semiring `⊗`; it is deliberately not offered.)
#[derive(Debug, Clone, Copy)]
pub(crate) struct Tropical;

impl Semiring for Tropical {
    type Value = Cost;
    fn zero(&self) -> Cost {
        Cost::Infinite
    }
    fn one(&self) -> Cost {
        Cost::Finite(0)
    }
    fn plus(&self, a: &Cost, b: &Cost) -> Cost {
        *a.min(b)
    }
    fn times(&self, a: &Cost, b: &Cost) -> Result<Cost, SemiringOverflow> {
        match (a, b) {
            (Cost::Infinite, _) | (_, Cost::Infinite) => Ok(Cost::Infinite),
            (Cost::Finite(x), Cost::Finite(y)) => {
                x.checked_add(*y).map(Cost::Finite).ok_or(SemiringOverflow {
                    left: *x,
                    right: *y,
                })
            }
        }
    }
    fn lift_weight(&self, weight: NonZeroU64) -> Cost {
        Cost::Finite(weight.get())
    }
}

// ─────────────────────────────────────────────────────────────────────────
// The derivation graph
// ─────────────────────────────────────────────────────────────────────────

/// One grounded rule application: `head ← premises`, by rule `label` of
/// the head's rule set, charging `weight`. The graph is semiring-agnostic
/// — one graph solves under [`Boolean`] and [`Tropical`] alike.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Derivation<K> {
    pub(crate) head: K,
    /// The per-head rule index (the same index the witness seam records),
    /// carried into certificates so an independent checker can resolve
    /// the rule and re-derive the instantiation from scratch.
    pub(crate) label: usize,
    /// The rule application's cost. `NonZeroU64` by construction: a
    /// zero-weight rule would let a min-cost cycle tie with itself and
    /// unfound certificate extraction (see [`extract_min_cost_proof`]).
    pub(crate) weight: NonZeroU64,
    /// The positive premises, in body order.
    pub(crate) premises: Vec<K>,
}

/// The grounded derivation hypergraph of one completed evaluation: ground
/// nodes (annotation `1`) and rule applications. Nodes are keyed by the
/// caller (`K` is `(PremiseSource, Tuple)` for the engine pipeline).
#[derive(Debug)]
pub(crate) struct DerivationGraph<K> {
    /// Ground nodes: EDB facts as attested by the rule bodies, plus the
    /// collapse boundary (tuples of aggregated / fixed-rule stores).
    pub(crate) facts: BTreeSet<K>,
    /// Every grounded rule application, in enumeration order (canonical:
    /// stratum, then store, then rule index, then the body's own
    /// deterministic iteration order).
    pub(crate) derivations: Vec<Derivation<K>>,
}

// A hand-written `Default` (an empty graph). The derived one would demand
// the needless bound `K: Default`, which the node type (`ProvNode`) does
// not satisfy.
impl<K> Default for DerivationGraph<K> {
    fn default() -> Self {
        Self {
            facts: BTreeSet::new(),
            derivations: Vec::new(),
        }
    }
}

impl<K: Ord + Clone + Debug> DerivationGraph<K> {
    /// Every node the graph mentions, in canonical order.
    pub(crate) fn nodes(&self) -> BTreeSet<K> {
        let mut nodes = self.facts.clone();
        for d in &self.derivations {
            nodes.insert(d.head.clone());
            for p in &d.premises {
                nodes.insert(p.clone());
            }
        }
        nodes
    }

    /// Refuse (typed) any premise that is neither a ground fact nor the
    /// head of some derivation: such a node would silently annotate to
    /// `0` and zero out every edge through it — a silent gap this check
    /// turns into a loud one. The engine builder calls this after
    /// enumeration; hand-built graphs in tests may skip it deliberately.
    pub(crate) fn check_closed(&self) -> Result<()> {
        let heads: BTreeSet<&K> = self.derivations.iter().map(|d| &d.head).collect();
        for d in &self.derivations {
            for p in &d.premises {
                if !self.facts.contains(p) && !heads.contains(p) {
                    return Err(ProvenanceInvariantError(
                        "a premise is neither a ground fact nor a derived head",
                    )
                    .into());
                }
            }
        }
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────
// The solver
// ─────────────────────────────────────────────────────────────────────────

/// The armed ceiling of the annotation fixpoint: how many full passes
/// over the derivation list the solver may take. Required by parameter —
/// there is no unbounded fixpoint in KyzoDB. A graph whose longest
/// dependency chain has `n` edges needs at most `n + 1` passes; the
/// shipped semirings' bounded `⊕`-chains guarantee some finite pass count
/// suffices, and the ceiling turns "not enough" into a typed refusal.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SolverBudget {
    pub(crate) max_passes: NonZeroU32,
}

impl SolverBudget {
    pub(crate) fn new(max_passes: NonZeroU32) -> Self {
        Self { max_passes }
    }
}

/// Compute every node's annotation: the least fixpoint of
/// `ann(head) = ⊕ over derivations of (weight ⊗ ⊗ premises)`, with ground
/// facts at `1` and everything else starting at `0`.
///
/// Deterministic by construction: the pass order is the derivation list's
/// order and the map is a `BTreeMap`; no iteration order depends on a
/// hash or a thread schedule.
pub(crate) fn solve<S: Semiring, K: Ord + Clone + Debug>(
    semiring: &S,
    graph: &DerivationGraph<K>,
    budget: &SolverBudget,
) -> Result<BTreeMap<K, S::Value>> {
    let mut ann: BTreeMap<K, S::Value> = graph
        .nodes()
        .into_iter()
        .map(|n| {
            let v = if graph.facts.contains(&n) {
                semiring.one()
            } else {
                semiring.zero()
            };
            (n, v)
        })
        .collect();

    let ceiling = budget.max_passes.get();
    for _pass in 0..ceiling {
        let mut changed = false;
        for d in &graph.derivations {
            let mut v = semiring.lift_weight(d.weight);
            for p in &d.premises {
                let pv = ann.get(p).ok_or(ProvenanceInvariantError(
                    "a premise vanished from the node set",
                ))?;
                v = semiring.times(&v, pv)?;
            }
            let old = ann.get(&d.head).ok_or(ProvenanceInvariantError(
                "a head vanished from the node set",
            ))?;
            let new = semiring.plus(old, &v);
            if new != *old {
                ann.insert(d.head.clone(), new);
                changed = true;
            }
        }
        if !changed {
            return Ok(ann);
        }
    }
    Err(ProvenanceLimitExceeded {
        dimension: "solver passes",
        spent: u64::from(ceiling),
        ceiling: u64::from(ceiling),
    }
    .into())
}

// ─────────────────────────────────────────────────────────────────────────
// Certificates
// ─────────────────────────────────────────────────────────────────────────

/// A checkable proof of one derived tuple: a tree of rule applications
/// grounding out in facts, with every step's claimed cost carried
/// explicitly so verification is pure arithmetic plus rule-instantiation
/// checks — no trust in the solver.
///
/// The boolean certificate is the unit-weight tropical one: a min-cost
/// tree under unit weights is in particular *a* derivation tree, which is
/// all existence needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProofNode<K> {
    /// A ground node: cost 0 by definition.
    Fact { node: K },
    /// A rule application: `derivation` indexes [`DerivationGraph::derivations`],
    /// `label` echoes its per-head rule index, `cost` is the claimed total
    /// (weight plus children), and `premises` are the children in body
    /// order.
    Step {
        node: K,
        derivation: usize,
        label: usize,
        cost: u64,
        premises: Vec<ProofNode<K>>,
    },
}

impl<K> ProofNode<K> {
    pub(crate) fn node(&self) -> &K {
        match self {
            ProofNode::Fact { node } | ProofNode::Step { node, .. } => node,
        }
    }
    pub(crate) fn cost(&self) -> u64 {
        match self {
            ProofNode::Fact { .. } => 0,
            ProofNode::Step { cost, .. } => *cost,
        }
    }
}

/// Extract a min-cost derivation tree for `target` from solved tropical
/// costs. Well-founded because weights are nonzero: every premise of a
/// cost-achieving derivation costs strictly less than its head, so the
/// recursion strictly descends in `u64` and terminates — no cycle can be
/// packaged into a certificate.
///
/// Deterministic: among cost-achieving derivations the one with the
/// lowest index wins.
pub(crate) fn extract_min_cost_proof<K: Ord + Clone + Debug>(
    graph: &DerivationGraph<K>,
    costs: &BTreeMap<K, Cost>,
    target: &K,
) -> Result<ProofNode<K>> {
    let target_cost = match costs.get(target) {
        Some(Cost::Finite(c)) => *c,
        Some(Cost::Infinite) => {
            return Err(NoDerivation(format!("{target:?} has no finite-cost derivation")).into());
        }
        None => return Err(NoDerivation(format!("{target:?} is not in the graph")).into()),
    };
    if graph.facts.contains(target) {
        return Ok(ProofNode::Fact {
            node: target.clone(),
        });
    }
    for (idx, d) in graph.derivations.iter().enumerate() {
        if d.head != *target {
            continue;
        }
        // Evaluate this edge at the solved costs; if it achieves the
        // node's cost it is a witness. Overflow here cannot achieve a
        // finite target cost, so treat it as "not this edge".
        let mut total = Some(d.weight.get());
        let mut premise_costs = Vec::with_capacity(d.premises.len());
        for p in &d.premises {
            match costs.get(p) {
                Some(Cost::Finite(c)) => {
                    premise_costs.push(*c);
                    total = total.and_then(|t| t.checked_add(*c));
                }
                _ => {
                    total = None;
                }
            }
            if total.is_none() {
                break;
            }
        }
        if total == Some(target_cost) {
            let premises = d
                .premises
                .iter()
                .map(|p| extract_min_cost_proof(graph, costs, p))
                .collect::<Result<Vec<_>>>()?;
            return Ok(ProofNode::Step {
                node: target.clone(),
                derivation: idx,
                label: d.label,
                cost: target_cost,
                premises,
            });
        }
    }
    // The fixpoint said `target_cost` but no edge achieves it: the graph
    // and the costs disagree — corruption, not a user error.
    Err(ProvenanceInvariantError("a solved min cost is achieved by no derivation").into())
}

/// Verify a proof against the graph: every `Fact` leaf is a ground node,
/// every `Step` cites a real derivation whose head and premises match the
/// tree exactly, and every claimed cost is the weight plus the children's
/// costs. Returns the verified root cost.
///
/// This is the *structural* half of verification (graph citation + cost
/// arithmetic). The *semantic* half — each step is a valid instantiation
/// of the named rule over its premises — is re-derived from scratch by
/// the independent checker in `query/provenance.rs`, which imports no
/// evaluator or solver symbol.
pub(crate) fn verify_proof<K: Ord + Clone + Debug>(
    proof: &ProofNode<K>,
    graph: &DerivationGraph<K>,
) -> Result<u64, BadCertificate> {
    match proof {
        ProofNode::Fact { node } => {
            if graph.facts.contains(node) {
                Ok(0)
            } else {
                Err(BadCertificate(format!(
                    "leaf {node:?} is not a ground fact"
                )))
            }
        }
        ProofNode::Step {
            node,
            derivation,
            label,
            cost,
            premises,
        } => {
            let d = graph.derivations.get(*derivation).ok_or_else(|| {
                BadCertificate(format!("derivation index {derivation} out of range"))
            })?;
            if d.head != *node {
                return Err(BadCertificate(format!(
                    "derivation {derivation} derives {:?}, not {node:?}",
                    d.head
                )));
            }
            if d.label != *label {
                return Err(BadCertificate(format!(
                    "derivation {derivation} is rule {}, certificate claims {label}",
                    d.label
                )));
            }
            if d.premises.len() != premises.len() {
                return Err(BadCertificate(format!(
                    "derivation {derivation} has {} premises, certificate carries {}",
                    d.premises.len(),
                    premises.len()
                )));
            }
            let mut total: u64 = d.weight.get();
            for (want, child) in d.premises.iter().zip(premises) {
                if child.node() != want {
                    return Err(BadCertificate(format!(
                        "premise mismatch: derivation {derivation} wants {want:?}, \
                         certificate supplies {:?}",
                        child.node()
                    )));
                }
                let child_cost = verify_proof(child, graph)?;
                total = total
                    .checked_add(child_cost)
                    .ok_or_else(|| BadCertificate("cost arithmetic overflows u64".to_string()))?;
            }
            if total != *cost {
                return Err(BadCertificate(format!(
                    "claimed cost {cost} ≠ verified cost {total} at {node:?}"
                )));
            }
            Ok(total)
        }
    }
}
