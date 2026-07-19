/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0). Provenance hooks and derivation-graph enumeration, split from
 * the semi-naive stratified evaluator.
 */

//! First-witness recording at the admission seam, and the derivation graph
//! over a completed fixpoint.

use std::collections::BTreeMap;
use std::ops::ControlFlow;

use miette::{Diagnostic, Result};
use thiserror::Error;

use crate::exec::plan::program::MagicSymbol;
use crate::exec::fixpoint::delta_store::{
    AdmissionSink, EpochStore, HeadPos, TempStoreCorruptRefuse, TupleInIter,
};
use crate::exec::fixpoint::eval::{
    Budget, EvalDefinition, EvalInvariantError, EvalProgram, HeadAggrKind, PremiseSource, Premises,
    RuleBody, FixedRuleEval, project_positions, store_of,
};
use crate::exec::provenance::semiring::{Derivation, DerivationGraph};
use kyzo_model::value::Tuple;

// ─────────────────────────────────────────────────────────────────────────
// Provenance: first-witness recording at the admission seam
// ─────────────────────────────────────────────────────────────────────────

/// The first witness of one admitted tuple: which rule of its set derived
/// it first this epoch, from which premise rows. `None` for rows without a
/// per-row derivation: normal-aggregation folds (their support is a whole
/// group), fixed-rule output, and the meet identity row.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Witness {
    pub(crate) store: MagicSymbol,
    pub(crate) tuple: Tuple,
    pub(crate) derivation: Option<(usize, Vec<Tuple>)>,
}

/// The witness table of one query: append-only, one entry per admission,
/// in admission order — which is canonical per store, per epoch, per
/// stratum, and therefore deterministic (asserted by the determinism
/// tests). Passing one to [`crate::exec::fixpoint::eval::stratified_evaluate`]
/// opts the query in; `None` evaluates through the `()` sink at zero cost.
#[derive(Debug, Default)]
pub(crate) struct WitnessTable {
    entries: Vec<Witness>,
}

impl WitnessTable {
    pub(crate) fn entries(&self) -> &[Witness] {
        &self.entries
    }
}

/// The pending witnesses of one rule set's epoch: candidate tuple (full
/// tuple for a regular store, group key for a meet store) → first
/// derivation. Built during (possibly parallel) rule evaluation, each map
/// owned by its own rule set; consumed at the sequential merge barrier.
pub(crate) type PendingWitnesses = BTreeMap<Tuple, (usize, Vec<Tuple>)>;

/// How a pending-witness map is keyed for one store at the merge barrier.
/// Meet groups project onto [`HeadPos`]s; regular stores key on the full
/// tuple. An `Option` residual for Meet key positions is unrepresentable (P025).
pub(crate) enum WitnessKeyMode<'a> {
    FullTuple,
    MeetGroup(&'a [HeadPos]),
}

/// The [`AdmissionSink`] that binds pending witnesses to admitted tuples
/// at the merge barrier. Meet stores key pending maps by the group
/// (projection onto non-aggregated head positions); regular stores key on
/// the full tuple — selected by [`WitnessKeyMode`], never an Option.
pub(crate) struct WitnessBinder<'a> {
    pub(crate) store: &'a MagicSymbol,
    pub(crate) pending: &'a PendingWitnesses,
    pub(crate) key_mode: WitnessKeyMode<'a>,
    pub(crate) table: &'a mut WitnessTable,
}

impl AdmissionSink for WitnessBinder<'_> {
    const RECORDING: bool = true;
    fn admit(&mut self, tuple: TupleInIter<'_>) -> Result<(), TempStoreCorruptRefuse> {
        let full = tuple.try_into_tuple()?;
        let derivation = match self.key_mode {
            WitnessKeyMode::FullTuple => self.pending.get(&full).cloned(),
            // Project the admitted head tuple onto the grouping positions to
            // recover the group key the pending map was recorded under — the
            // same projection eval used at derivation and the store used to
            // fold, so a non-suffix layout binds exactly as a suffix one.
            WitnessKeyMode::MeetGroup(positions) => self
                .pending
                .get(&project_positions(full.as_slice(), positions))
                .cloned(),
        };
        self.table.entries.push(Witness {
            store: self.store.clone(),
            tuple: full,
            derivation,
        });
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Provenance: the derivation graph over a completed fixpoint
// ─────────────────────────────────────────────────────────────────────────

/// Provenance was requested where it cannot be honestly computed. Typed:
/// the engine refuses rather than returning a graph with silent gaps.
#[derive(Debug, Error, Diagnostic)]
#[error("provenance unavailable for '{store}': {reason}")]
#[diagnostic(
    code(provenance::unsupported),
    help(
        "provenance needs every rule body to attribute its premises and \
         every premised store to stay live through the final stratum"
    )
)]
pub(crate) struct ProvenanceUnsupported {
    pub(crate) store: MagicSymbol,
    pub(crate) reason: &'static str,
}

/// A provenance graph node: which source a tuple belongs to. Two
/// relations may hold byte-identical tuples; the source keeps them
/// distinct.
pub(crate) type ProvNode = (PremiseSource, Tuple);

/// Enumerate every grounded derivation the completed stores admit and
/// build the semiring derivation graph, for
/// [`crate::exec::provenance::semiring::solve`].
///
/// - Only plain ([`HeadAggrKind::None`](crate::exec::fixpoint::eval::HeadAggrKind::None))
///   rule sets contribute
///   derivations. Meet- and normal-aggregated heads and fixed rules are
///   the **collapse boundary**: aggregation folds and opaque algorithms
///   are not semiring operations, so their stores' tuples enter the graph
///   as ground facts (annotation `1`) and full provenance is claimed only
///   for the positive plain-rule fragment above them. Negated literals
///   contribute no premise (they are absent from [`Premises::Rows`]).
/// - `stores` is the map [`crate::exec::fixpoint::eval::stratified_evaluate_with_stores`]
///   returned; a
///   store a body premises that is absent (dropped by lifetimes, or never
///   retained) is a typed [`ProvenanceUnsupported`] refusal.
/// - Every `Rule`-sourced premise row is verified to be in its store's
///   total — a mismatch is an invariant error, never a silently wrong
///   graph. `Fact`-sourced rows are attested by the body that read them;
///   the independent certificate checker re-verifies them from the model.
/// - `derivation_ceiling` arms the enumeration (grounded derivations can
///   be quadratic in store rows); crossing it is the typed
///   [`ProvenanceLimitExceeded`] refusal. `budget` threads the same
///   kill/deadline interrupts as evaluation.
/// - `weights` prices one rule application, keyed by store and per-head
///   rule index; the tropical semiring charges it, the boolean one
///   ignores it. Unit weights make cost = number of rule firings.
/// - The enumeration is limiter-blind: it re-derives from the completed
///   stores and does not replay a `:limit` early stop.
///
/// [`ProvenanceLimitExceeded`]: crate::exec::provenance::semiring::ProvenanceLimitExceeded
pub(crate) fn provenance_graph<R: RuleBody, F: FixedRuleEval>(
    program: &EvalProgram<R, F>,
    stores: &BTreeMap<MagicSymbol, EpochStore>,
    budget: &Budget,
    derivation_ceiling: std::num::NonZeroU64,
    weights: &dyn Fn(&MagicSymbol, usize) -> std::num::NonZeroU64,
) -> Result<DerivationGraph<ProvNode>> {
    let mut graph: DerivationGraph<ProvNode> = DerivationGraph::default();
    let ceiling = derivation_ceiling.get();
    let mut spent: u64 = 0;

    // Pre-declare every retained store tuple so stratified re-derive may
    // admit edges whose rule-premises appear later in enumeration order
    // (P104: premises must be known at add_derivation).
    for (name, store) in stores {
        for t in store.all_iter()? {
            graph.declare((PremiseSource::Rule(name.clone()), t.try_into_tuple()?));
        }
    }

    for stratum in &program.strata {
        for (name, def) in &stratum.defs {
            let rule_set = match def {
                EvalDefinition::Rules(rule_set) if rule_set.kind == HeadAggrKind::None => rule_set,
                // The collapse boundary: aggregated and fixed-rule stores
                // ground out. (An absent store here is fine — nothing can
                // premise it without tripping the liveness refusal below.)
                EvalDefinition::Rules(_) | EvalDefinition::Fixed { .. } => {
                    if let Some(store) = stores.get(name) {
                        for t in store.all_iter()? {
                            graph.add_fact((PremiseSource::Rule(name.clone()), t.try_into_tuple()?));
                        }
                    }
                    continue;
                }
            };
            // Interrupt poll only (in_flight 0): enumeration spend is governed
            // by the provenance derivation ceiling below, not the derived-tuple
            // meter, so this ticker must not contribute mid-epoch spend.
            let mut ticker = budget.ticker(0, name);
            for (rule_n, body) in rule_set.bodies.iter().enumerate() {
                let sources = body
                    .premise_sources()
                    .ok_or_else(|| ProvenanceUnsupported {
                        store: name.clone(),
                        reason: "a rule body does not attribute its premises",
                    })?;
                for dep in body.contained_rules().values() {
                    if !stores.contains_key(dep) {
                        return Err(ProvenanceUnsupported {
                            store: dep.clone(),
                            reason: "a premised store was not retained to the final stratum",
                        }
                        .into());
                    }
                }
                let weight = weights(name, rule_n);
                body.for_each_derivation(stores, None, true, &mut |head, premises| {
                    ticker.tick(0)?;
                    spent += 1;
                    if spent > ceiling {
                        return Err(crate::exec::provenance::semiring::ProvenanceLimitExceeded {
                            dimension: "enumerated derivations",
                            spent,
                            ceiling,
                        }
                        .into());
                    }
                    let rows = premises.to_rows();
                    if rows.len() != sources.len() {
                        return Err(EvalInvariantError(
                            "premise rows disagree with the body's attribution",
                        )
                        .into());
                    }
                    let mut premise_nodes = Vec::with_capacity(rows.len());
                    for (src, row) in sources.iter().zip(rows) {
                        if let PremiseSource::Rule(sym) = src {
                            let dep = store_of(stores, sym)?;
                            let present = dep
                                .prefix_iter(&row)?
                                .next()
                                .is_some_and(|t| t.try_into_tuple().ok() == Some(row.clone()));
                            if !present {
                                return Err(EvalInvariantError(
                                    "a premise row is missing from its attributed store",
                                )
                                .into());
                            }
                        } else {
                            graph.add_fact((src.clone(), row.clone()));
                        }
                        premise_nodes.push((src.clone(), row));
                    }
                    graph.add_derivation(Derivation {
                        head: (
                            PremiseSource::Rule(name.clone()),
                            Tuple::from_vec(head.into_owned()),
                        ),
                        label: rule_n,
                        weight,
                        premises: premise_nodes,
                    })?;
                    Ok(ControlFlow::Continue(()))
                })?;
            }
        }
    }
    graph.check_closed()?;
    Ok(graph)
}
