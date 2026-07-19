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
//! - [`BooleanAnn`] — existence. Its support must equal the engine's set
//!   semantics (the evaluator's own fixpoint), proven by differential
//!   against the sealed oracle (`query/provenance.rs`).
//! - [`TropicalAnn`] — min-plus over [`Cost`]: the cheapest derivation, where
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
//! degrades into them: annotations are sealed products
//! ([`BooleanAnn`] / [`TropicalAnn`]) so a kind mismatch does not compile,
//! and the solver is armed with a pass ceiling ([`SolverBudget`]) so a
//! non-stabilizing annotation chain refuses (typed) instead of hanging.
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
//! [`RuleBody`]: crate::exec::fixpoint::eval::RuleBody
//! [`WitnessTable`]: crate::exec::provenance::eval::WitnessTable

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

/// A proof failed verification. The variant names the first offending
/// step; a certificate is all-or-nothing.
#[derive(Debug, Error, Diagnostic, PartialEq, Eq)]
#[diagnostic(code(provenance::bad_certificate))]
pub(crate) enum BadCertificate {
    #[error("provenance certificate rejected: leaf is not a ground fact")]
    NotGroundFact,
    #[error("provenance certificate rejected: derivation index out of range")]
    DerivationOutOfRange,
    #[error("provenance certificate rejected: derivation head mismatch")]
    HeadMismatch,
    #[error("provenance certificate rejected: rule label mismatch")]
    LabelMismatch,
    #[error("provenance certificate rejected: premise arity mismatch")]
    PremiseArityMismatch,
    #[error("provenance certificate rejected: premise node mismatch")]
    PremiseMismatch,
    #[error("provenance certificate rejected: cost arithmetic overflows u64")]
    CostOverflow,
    #[error("provenance certificate rejected: claimed cost disagrees with verified cost")]
    CostMismatch,
}

/// Certificate extraction was asked for an underivable tuple (annotation
/// `0` / infinite cost), or for a node the graph does not contain.
#[derive(Debug, Error, Diagnostic, PartialEq, Eq)]
#[diagnostic(code(provenance::no_derivation))]
pub(crate) enum NoDerivation {
    #[error("no derivation to certify: target has no finite-cost derivation")]
    NoFiniteCost,
    #[error("no derivation to certify: target is not in the graph")]
    MissingNode,
}

/// A cross-stage invariant the graph construction should have made
/// impossible (e.g. "a solved min cost is achieved by some edge").
/// Surfaced as an error, never an abort.
#[derive(Debug, Error, Diagnostic)]
#[error("provenance invariant violated: {0}")]
#[diagnostic(code(provenance::invariant), help("This is a bug. Please report it."))]
pub(crate) struct ProvenanceInvariantError(pub(crate) &'static str);

// ─────────────────────────────────────────────────────────────────────────
// The semiring algebra (sealed enum — not an open trait)
// ─────────────────────────────────────────────────────────────────────────

/// Which commutative semiring annotates the derivation graph.
///
/// Exactly the two **idempotent** algebras whose fixpoints are finite.
/// Counting/polynomial are refused at this door — they are not variants.
/// Each variant's annotation is a sealed product type
/// ([`BooleanAnn`] / [`TropicalAnn`]): kind mismatch does not compile.
///
/// Laws (asserted on randomized values by the axiom tests in
/// `query/provenance.rs`):
///
/// - `⊕` associative, commutative, identity `0`;
/// - `⊗` associative, commutative, identity `1`, annihilator `0`
///   (`0 ⊗ a = 0`);
/// - `⊗` distributes over `⊕`.
///
/// **Solver contract beyond the axioms**: `⊕` is idempotent (`a ⊕ a = a`)
/// and every `⊕`-chain stabilizes after finitely many strict changes —
/// true of [`BooleanAnn`] (one flip) and [`TropicalAnn`] (a strictly
/// decreasing `u64` chain is finite). The solver's pass ceiling turns any
/// non-stabilizing chain into a typed refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Semiring {
    /// `({⊥,⊤}, ∨, ∧, ⊥, ⊤)`: does a derivation exist. Support equals the
    /// engine's set semantics — asserted by differential against the oracle.
    Boolean,
    /// `(ℕ∪{∞}, min, +, ∞, 0)`: cheapest derivation cost. Derivation
    /// *depth* is deliberately not offered (min-max is not a semiring `⊗`).
    Tropical,
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

/// The algebra operations every sealed annotation product exposes. The
/// associated value *is* the annotation — there is no independent
/// `Annotation` enum that can disagree with its [`Semiring`].
pub(crate) trait AnnAlgebra: Copy + Clone + PartialEq + Eq + Ord + Debug + Sized {
    fn zero() -> Self;
    fn one() -> Self;
    fn plus(self, other: Self) -> Self;
    fn times(self, other: Self) -> Result<Self, SemiringOverflow>;
    fn lift_weight(weight: NonZeroU64) -> Self;
}

/// Sealed product: [`Semiring::Boolean`] × existence bit. Ops are total
/// on this type alone — a tropical value cannot be passed here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct BooleanAnn(bool);

impl BooleanAnn {
    pub(crate) fn new(present: bool) -> Self {
        Self(present)
    }

    pub(crate) fn get(self) -> bool {
        self.0
    }
}

impl AnnAlgebra for BooleanAnn {
    fn zero() -> Self {
        Self(false)
    }

    fn one() -> Self {
        Self(true)
    }

    fn plus(self, other: Self) -> Self {
        Self(self.0 || other.0)
    }

    fn times(self, other: Self) -> Result<Self, SemiringOverflow> {
        Ok(Self(self.0 && other.0))
    }

    fn lift_weight(_weight: NonZeroU64) -> Self {
        Self(true)
    }
}

/// Sealed product: [`Semiring::Tropical`] × [`Cost`]. Ops are total on
/// this type alone — a boolean value cannot be passed here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct TropicalAnn(Cost);

impl TropicalAnn {
    pub(crate) fn new(cost: Cost) -> Self {
        Self(cost)
    }

    pub(crate) fn cost(self) -> Cost {
        self.0
    }
}

impl AnnAlgebra for TropicalAnn {
    fn zero() -> Self {
        Self(Cost::Infinite)
    }

    fn one() -> Self {
        Self(Cost::Finite(0))
    }

    fn plus(self, other: Self) -> Self {
        Self(self.0.min(other.0))
    }

    fn times(self, other: Self) -> Result<Self, SemiringOverflow> {
        match (self.0, other.0) {
            (Cost::Infinite, _) | (_, Cost::Infinite) => Ok(Self(Cost::Infinite)),
            (Cost::Finite(l), Cost::Finite(r)) => l
                .checked_add(r)
                .map(|s| Self(Cost::Finite(s)))
                .ok_or(SemiringOverflow { left: l, right: r }),
        }
    }

    fn lift_weight(weight: NonZeroU64) -> Self {
        Self(Cost::Finite(weight.get()))
    }
}

// ─────────────────────────────────────────────────────────────────────────
// The derivation graph
// ─────────────────────────────────────────────────────────────────────────

/// One grounded rule application: `head ← premises`, by rule `label` of
/// the head's rule set, charging `weight`. The graph is semiring-agnostic
/// — one graph solves under [`Semiring::Boolean`] and [`Semiring::Tropical`]
/// alike.
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

/// Typed index into [`DerivationGraph::derivations`] — certificates name
/// steps by this id, never a bare `usize` (P104).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct DerivationId(usize);

impl DerivationId {
    pub(crate) fn index(self) -> usize {
        self.0
    }
}

/// The grounded derivation hypergraph of one completed evaluation: ground
/// nodes (annotation `1`) and rule applications. Nodes are keyed by the
/// caller (`K` is `(PremiseSource, Tuple)` for the engine pipeline).
///
/// DAG-by-construction (P104): edges are admitted only through
/// [`Self::add_derivation`], which requires every premise already
/// [`Self::declare`]d or [`Self::add_fact`]ed (or a prior head), refuses
/// self-loops, and refuses any edge that would create a cycle. Free struct
/// literals cannot forge an open or cyclic graph.
#[derive(Debug)]
pub(crate) struct DerivationGraph<K> {
    /// Ground nodes: EDB facts as attested by the rule bodies, plus the
    /// collapse boundary (tuples of aggregated / fixed-rule stores).
    facts: BTreeSet<K>,
    /// Every grounded rule application, in enumeration order (canonical:
    /// stratum, then store, then rule index, then the body's own
    /// deterministic iteration order).
    derivations: Vec<Derivation<K>>,
    /// Nodes known for premise checks: facts ∪ declared ∪ derivation heads.
    known: BTreeSet<K>,
}

// A hand-written `Default` (an empty graph). The derived one would demand
// the needless bound `K: Default`, which the node type (`ProvNode`) does
// not satisfy.
impl<K> Default for DerivationGraph<K> {
    fn default() -> Self {
        Self {
            facts: BTreeSet::new(),
            derivations: Vec::new(),
            known: BTreeSet::new(),
        }
    }
}

impl<K: Ord + Clone + Debug> DerivationGraph<K> {
    /// Admit a ground fact (annotation `1`).
    pub(crate) fn add_fact(&mut self, node: K) {
        self.known.insert(node.clone());
        self.facts.insert(node);
    }

    /// Declare a node known for premise checks without making it a ground
    /// fact (annotation still starts at `0` until a derivation fires).
    /// Used to pre-seat completed-store heads before stratified re-derive
    /// admits edges in non-topo list order.
    pub(crate) fn declare(&mut self, node: K) {
        self.known.insert(node);
    }

    /// Admit one grounded rule application. Refuses a self-loop, any
    /// premise not yet in [`Self::known`], or any edge that would create a
    /// cycle (following premise→head edges, `head` must not already reach a
    /// premise) — the graph stays a closed DAG by construction.
    pub(crate) fn add_derivation(&mut self, d: Derivation<K>) -> Result<DerivationId> {
        if d.premises.iter().any(|p| p == &d.head) {
            return Err(ProvenanceInvariantError(
                "a derivation premise equals its head (self-loop)",
            )
            .into());
        }
        for p in &d.premises {
            if !self.known.contains(p) {
                return Err(ProvenanceInvariantError(
                    "a premise is neither a ground fact nor a derived head",
                )
                .into());
            }
            if self.reaches(&d.head, p) {
                return Err(ProvenanceInvariantError(
                    "admitting this derivation would create a cycle",
                )
                .into());
            }
        }
        self.known.insert(d.head.clone());
        let id = DerivationId(self.derivations.len());
        self.derivations.push(d);
        Ok(id)
    }

    /// Whether `from` can reach `to` following existing premise→head edges.
    fn reaches(&self, from: &K, to: &K) -> bool {
        if from == to {
            return true;
        }
        let mut stack = vec![from.clone()];
        let mut seen = BTreeSet::new();
        while let Some(n) = stack.pop() {
            if !seen.insert(n.clone()) {
                continue;
            }
            for d in &self.derivations {
                if d.premises.iter().any(|p| p == &n) {
                    if d.head == *to {
                        return true;
                    }
                    stack.push(d.head.clone());
                }
            }
        }
        false
    }

    pub(crate) fn facts(&self) -> &BTreeSet<K> {
        &self.facts
    }

    pub(crate) fn derivations(&self) -> &[Derivation<K>] {
        &self.derivations
    }

    pub(crate) fn derivation(&self, id: DerivationId) -> Option<&Derivation<K>> {
        self.derivations.get(id.0)
    }

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

    /// Defense-in-depth closure check (construction already enforces this).
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
/// `A` is the sealed product ([`BooleanAnn`] / [`TropicalAnn`]) — the
/// algebra is fixed by the type parameter, so a kind mismatch cannot
/// arise at the ops boundary.
///
/// Deterministic by construction: the pass order is the derivation list's
/// order and the map is a `BTreeMap`; no iteration order depends on a
/// hash or a thread schedule.
pub(crate) fn solve<A: AnnAlgebra, K: Ord + Clone + Debug>(
    graph: &DerivationGraph<K>,
    budget: &SolverBudget,
) -> Result<BTreeMap<K, A>> {
    let mut ann: BTreeMap<K, A> = graph
        .nodes()
        .into_iter()
        .map(|n| {
            let v = if graph.facts().contains(&n) {
                A::one()
            } else {
                A::zero()
            };
            (n, v)
        })
        .collect();

    let ceiling = budget.max_passes.get();
    for _pass in 0..ceiling {
        let mut changed = false;
        for d in graph.derivations() {
            let mut v = A::lift_weight(d.weight);
            for p in &d.premises {
                let pv = ann.get(p).ok_or(ProvenanceInvariantError(
                    "a premise vanished from the node set",
                ))?;
                v = v.times(*pv)?;
            }
            let old = ann.get(&d.head).ok_or(ProvenanceInvariantError(
                "a head vanished from the node set",
            ))?;
            let new = old.plus(v);
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

/// Project a tropical annotation map to [`Cost`] values. The input type
/// proves the map is tropical — no runtime kind check.
pub(crate) fn as_cost_map<K: Ord + Clone>(ann: &BTreeMap<K, TropicalAnn>) -> BTreeMap<K, Cost> {
    ann.iter().map(|(k, v)| (k.clone(), v.cost())).collect()
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
    /// A rule application: `derivation` is a [`DerivationId`] into the
    /// graph, `label` echoes its per-head rule index, `cost` is the claimed
    /// total (weight plus children), and `premises` are the children in
    /// body order.
    Step {
        node: K,
        derivation: DerivationId,
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
            return Err(NoDerivation::NoFiniteCost.into());
        }
        None => return Err(NoDerivation::MissingNode.into()),
    };
    if graph.facts().contains(target) {
        return Ok(ProofNode::Fact {
            node: target.clone(),
        });
    }
    for (idx, d) in graph.derivations().iter().enumerate() {
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
                derivation: DerivationId(idx),
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
            if graph.facts().contains(node) {
                Ok(0)
            } else {
                Err(BadCertificate::NotGroundFact)
            }
        }
        ProofNode::Step {
            node,
            derivation,
            label,
            cost,
            premises,
        } => {
            let d = graph
                .derivation(*derivation)
                .ok_or(BadCertificate::DerivationOutOfRange)?;
            if d.head != *node {
                return Err(BadCertificate::HeadMismatch);
            }
            if d.label != *label {
                return Err(BadCertificate::LabelMismatch);
            }
            if d.premises.len() != premises.len() {
                return Err(BadCertificate::PremiseArityMismatch);
            }
            let mut total: u64 = d.weight.get();
            for (want, child) in d.premises.iter().zip(premises) {
                if child.node() != want {
                    return Err(BadCertificate::PremiseMismatch);
                }
                let child_cost = verify_proof(child, graph)?;
                total = total
                    .checked_add(child_cost)
                    .ok_or(BadCertificate::CostOverflow)?;
            }
            if total != *cost {
                return Err(BadCertificate::CostMismatch);
            }
            Ok(total)
        }
    }
}
