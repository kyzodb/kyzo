/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Two verify doors live here:
//!
//! ## Query-answer `::verify` — provenance door
//!
//! [`crate::parse::sys::SysOp::Verify`] / [`Engine::verify_input_program`] runs
//! the query through the production evaluator with stores retained, builds the
//! derivation graph via [`crate::exec::provenance::eval::provenance_graph`],
//! solves the tropical semiring, extracts min-cost certificates, and checks
//! them with [`crate::exec::provenance::semiring::verify_proof`] (structural
//! certificate check — imports no evaluator symbol). Outcomes are
//! [`VerifyOutcome::Match`], [`VerifyOutcome::BudgetRefused`], or
//! [`VerifyOutcome::Mismatch`] (reproducible bundle), rendered as NamedRows
//! `["status", "summary", "detail"]`.
//!
//! Unsupported constructs (mutations, `:order`/`:limit`/`:offset`,
//! `@spans`/`@delta`/`@delta_sys` interval-derivation reads, bodies that
//! cannot attribute premises) are named [`VerifyOutcome::Unsupported`] — never
//! a silent pass.
//!
//! The oracle-differential corpus that once lived here is re-homed in
//! `kyzo-trials` (`verify_differential`); rewriting that corpus onto this
//! provenance door is a follow-on pass (parent), not this wire.
//!
//! ## Root tamper evidence (story #289)
//!
//! [`verify`] independently recomputes a plaintext-canonical [`StateRoot`]
//! from store contents and compares it to the stored [`RootChain`] tip via
//! [`as_of_root`] / [`roots_equal_at_cut`]. The expected digest is always
//! looked up from the chain; the observed digest is always a cold rescan —
//! a caller-supplied root is never an input to this door. Separate from
//! query-answer `::verify`.

use std::collections::{BTreeMap, BTreeSet};
use std::num::{NonZeroU32, NonZeroU64};

use miette::{Diagnostic, Result, miette};
use thiserror::Error;

use crate::data::json::NamedRows;
use crate::exec::fixpoint::delta_store::TupleInIter;
use crate::exec::fixpoint::eval::{PremiseSource, RowLimit, stratified_evaluate_with_stores};
use crate::exec::plan::compile::stratified_magic_compile;
use crate::exec::provenance::eval::{ProvenanceUnsupported, provenance_graph};
use crate::exec::provenance::semiring::{
    Cost, ProvenanceLimitExceeded, SolverBudget, TropicalAnn, as_cost_map, extract_min_cost_proof,
    solve, verify_proof,
};
use crate::parse::{Script, parse_script};
use crate::rules::contract::{CancelAuthority, SessionFixedRule};
use crate::session::current_validity;
use crate::session::db::{
    DEFAULT_DERIVED_TUPLE_CEILING, DEFAULT_EPOCH_CEILING, Engine, ScriptOptions, SessionNormalizer,
    SessionTx, SessionView, build_budget,
};
use crate::store::merkle::{
    ChainedStateRoot, RootChain, StateRoot, as_of_root, link_at_cut, roots_equal_at_cut, state_root,
};
use crate::store::{CommitOrdinal, ReadTx, Storage};
use kyzo_model::program::InputProgram;
use kyzo_model::program::rule::{
    InputAtom, InputInlineRulesOrFixed, InputNamedFieldRelationApplyAtom, InputRelationApplyAtom,
    ValidityClause,
};
use kyzo_model::value::{DataValue, Tuple, ValidityTs};

// ─────────────────────────────────────────────────────────────────────────
// Root tamper evidence (#289)
// ─────────────────────────────────────────────────────────────────────────

/// Outcome of root tamper-evidence [`verify`]: intact chain match, or a
/// reproducible mismatch between the stored tip and an independent rescan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootVerifyOutcome {
    /// Recomputed content seals to the stored [`RootChain`] tip at the cut.
    Intact { root: StateRoot },
    /// Store contents no longer seal to the chain tip — tamper or rollback.
    Tampered {
        expected: StateRoot,
        recomputed: StateRoot,
    },
}

/// Tamper-evidence door: independently recompute the store's plaintext-
/// canonical content root and compare it to the stored [`RootChain`] at
/// `cut` via [`as_of_root`].
///
/// Authority split (load-bearing):
/// - **expected** — only from [`as_of_root`] / [`RootChain`] (stored prior);
/// - **observed** — only from a cold [`state_root`] rescan of `tx`.
///
/// A caller-supplied [`StateRoot`] is not a parameter of this door and
/// cannot become the expected digest. Forged / swapped store bytes surface
/// as [`RootVerifyOutcome::Tampered`].
///
/// Live caller: [`crate::session::db::Engine::verify_root_chain`].
pub(crate) fn verify(
    tx: &impl ReadTx,
    chain: &RootChain,
    cut: CommitOrdinal,
    budget: std::num::NonZeroU64,
) -> Result<RootVerifyOutcome> {
    let expected = as_of_root(chain, cut)?;
    let link = link_at_cut(chain, cut)?;
    // INVARIANT(root_tip_agreement): as_of_root and link_at_cut read the same
    // chain tip for `cut`.
    let content = StateRoot::from_merkle(state_root(tx, budget)?);
    let recomputed = ChainedStateRoot::mint(
        link.store_id(),
        link.fence_epoch(),
        link.commit_ordinal(),
        content,
        link.predecessor_root(),
        link.link(),
    )?
    .root();

    if roots_equal_at_cut(expected, recomputed) {
        Ok(RootVerifyOutcome::Intact { root: expected })
    } else {
        Ok(RootVerifyOutcome::Tampered {
            expected,
            recomputed,
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Query-answer ::verify — provenance door
// ─────────────────────────────────────────────────────────────────────────

/// Outcome of one provenance-backed `::verify` run. Never a bare bool: a
/// MATCH, a budgeted refusal, a reproducible MISMATCH bundle, or a named
/// unsupported construct.
/// Payload for [`VerifyOutcome::Mismatch`] — boxed so the outcome enum stays small.
#[derive(Debug, Clone)]
pub struct VerifyMismatch {
    pub program: MismatchProgram,
    pub evaluated: BTreeSet<Tuple>,
    pub provenance: BTreeSet<Tuple>,
    pub certificate: Option<String>,
}

#[derive(Debug, Clone)]
pub enum VerifyOutcome {
    /// Evaluated entry answers equal provenance support; every answer has
    /// a structurally verified min-cost certificate.
    Match { row_count: usize },
    /// Evaluated answers disagree with provenance support, or a certificate
    /// fails `verify_proof`. Carries both sets plus the typed program for
    /// reproduction.
    Mismatch(Box<VerifyMismatch>),
    /// A construct this door refuses rather than silently mistranslating.
    Unsupported { reason: VerifyUnsupported },
    /// A provenance enumeration or solver ceiling was crossed.
    BudgetRefused { reason: ProvenanceLimitExceeded },
}

/// Typed program carried by [`VerifyOutcome::Mismatch`].
#[derive(Debug, Clone)]
pub struct MismatchProgram(pub(crate) InputProgram);

impl std::fmt::Display for MismatchProgram {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

/// Named reason [`VerifyOutcome::Unsupported`] carries.
#[derive(Debug, Clone, PartialEq, Eq, Error, Diagnostic)]
pub enum VerifyUnsupported {
    #[error(
        "::verify supports single read queries only, not sys ops or \
         imperative scripts"
    )]
    #[diagnostic(code(verify::not_single_read))]
    NotSingleRead,
    #[error("::verify supports pure read queries only, not mutations")]
    #[diagnostic(code(verify::mutation))]
    Mutation,
    #[error(
        ":order / :limit / :offset are not supported by this cut of \
         ::verify — it compares full, unordered answer sets under provenance"
    )]
    #[diagnostic(code(verify::order_limit_offset))]
    OrderLimitOffset,
    #[error(
        "relation atom '{name}' is an interval-derivation (@spans) or diff \
         (@delta/@delta_sys) read: these bind an extra column beyond the \
         relation's own arity — not supported by this cut of ::verify"
    )]
    #[diagnostic(code(verify::interval_derivation))]
    IntervalDerivation { name: String },
    #[error("provenance unavailable: {reason}")]
    #[diagnostic(code(verify::provenance_unavailable))]
    ProvenanceUnavailable { reason: &'static str },
}

impl From<VerifyUnsupported> for VerifyOutcome {
    fn from(reason: VerifyUnsupported) -> Self {
        VerifyOutcome::Unsupported { reason }
    }
}

impl VerifyOutcome {
    /// Product-surface rendering for `::verify { … }` — one row,
    /// `["status", "summary", "detail"]`.
    pub(crate) fn into_named_rows(self) -> NamedRows {
        let (status, summary, detail) = match self {
            VerifyOutcome::Match { row_count } => {
                ("match", format!("{row_count} row(s) agree"), String::new())
            }
            VerifyOutcome::Mismatch(m) => {
                let mut detail = format!(
                    "program:\n{}\nevaluated: {:?}\nprovenance: {:?}",
                    m.program, m.evaluated, m.provenance
                );
                if let Some(cert) = &m.certificate {
                    // String's Write impl is infallible — format, don't match Err away.
                    detail.push_str(&format!("\ncertificate: {cert}"));
                }
                (
                    "mismatch",
                    format!(
                        "evaluated {} row(s) vs provenance {} row(s)",
                        m.evaluated.len(),
                        m.provenance.len()
                    ),
                    detail,
                )
            }
            VerifyOutcome::Unsupported { reason } => {
                ("unsupported", reason.to_string(), String::new())
            }
            VerifyOutcome::BudgetRefused { reason } => {
                ("refused", reason.to_string(), String::new())
            }
        };
        NamedRows::verify_status_row(status, summary, detail)
    }
}

/// First stored-relation atom carrying `@spans` / `@delta` / `@delta_sys`,
/// if any — the language-door shape provenance `::verify` refuses rather
/// than silently matching.
fn first_interval_derivation(program: &InputProgram) -> Option<String> {
    for (_name, def) in program.iter_all() {
        let InputInlineRulesOrFixed::Rules { rules } = def else {
            continue;
        };
        for rule in rules {
            if let Some(rel) = rule.body.iter().find_map(atom_interval_derivation) {
                return Some(rel);
            }
        }
    }
    None
}

fn atom_interval_derivation(atom: &InputAtom) -> Option<String> {
    match atom {
        InputAtom::Relation {
            inner: InputRelationApplyAtom { name, validity, .. },
        }
        | InputAtom::NamedFieldRelation {
            inner: InputNamedFieldRelationApplyAtom { name, validity, .. },
        } => match validity {
            Some(ValidityClause::Spans { .. } | ValidityClause::Delta { .. }) => {
                Some(name.name.to_string())
            }
            Some(ValidityClause::At(_)) | None => None,
        },
        InputAtom::Negation { inner, .. } => atom_interval_derivation(inner),
        InputAtom::Conjunction { inner, .. } | InputAtom::Disjunction { inner, .. } => {
            inner.iter().find_map(atom_interval_derivation)
        }
        InputAtom::Rule { .. }
        | InputAtom::Predicate { .. }
        | InputAtom::Unification { .. }
        | InputAtom::Search { .. } => None,
    }
}

impl<S: Storage> Engine<S> {
    /// Rust API: parse `payload` and run provenance-backed `::verify`.
    pub fn verify_script(
        &self,
        payload: &str,
        params: BTreeMap<String, DataValue>,
        options: ScriptOptions,
    ) -> Result<VerifyOutcome> {
        let cur_vld = current_validity()?;
        match parse_script(payload, &params, cur_vld)? {
            Script::Query(prog) => self.verify_input_program(prog, cur_vld, &options),
            Script::Sys(_) | Script::Imperative(_) => Ok(VerifyUnsupported::NotSingleRead.into()),
        }
    }

    /// `::verify { … }` door (`SysOp::Verify`): compile → eval with stores
    /// retained → [`provenance_graph`] → tropical solve →
    /// [`extract_min_cost_proof`] + [`verify_proof`] per entry answer.
    pub(crate) fn verify_input_program(
        &self,
        program: InputProgram,
        cur_vld: ValidityTs,
        options: &ScriptOptions,
    ) -> Result<VerifyOutcome> {
        if program.out_opts().store_relation.is_some() {
            return Ok(VerifyUnsupported::Mutation.into());
        }
        if !program.out_opts().sorters.is_empty()
            || program.out_opts().limit.is_some()
            || program.out_opts().offset.is_some()
        {
            return Ok(VerifyUnsupported::OrderLimitOffset.into());
        }
        if let Some(name) = first_interval_derivation(&program) {
            return Ok(VerifyUnsupported::IntervalDerivation { name }.into());
        }

        let out_opts = program.out_opts().clone();
        let mismatch_program = MismatchProgram(program.clone());
        let tx = SessionTx::new_read(self.store.read_tx()?, options.clone());

        let (_auth, cancel) = CancelAuthority::arm();
        let view = SessionView {
            store: &tx.store,
            temp: &tx.temp,
        };
        let mut normalizer = SessionNormalizer::new(view, cancel.clone());
        let (nf, _) =
            crate::exec::plan::program::into_normalized_program(program, &mut normalizer)?;
        let (strat, mut lifetimes) = nf.into_stratified_program()?;
        let magic = strat.magic_sets_rewrite(&view)?;
        let compiled = stratified_magic_compile(&tx.store, magic)?;
        let eval_prog = crate::exec::plan::compile::bind_for_eval(
            &compiled,
            &tx.store,
            crate::project::current::Segments(Some(&self.segments)),
            &mut |app| Ok(SessionFixedRule::new(app, view, cancel.clone())),
        )?;

        match cur_vld {
            value => core::mem::drop(value),
        }
        let budget = build_budget(options, &out_opts, cancel)?;

        // Provenance needs every rule store live through the final stratum.
        let keep_until = crate::rules::convert::saturating_sub_usize(eval_prog.strata.len(), 1);
        for stratum in &eval_prog.strata {
            for name in stratum.defs.keys() {
                lifetimes.note_use(name.clone(), keep_until);
            }
        }

        let (outcome, mut stores) = stratified_evaluate_with_stores(
            &eval_prog,
            &lifetimes,
            RowLimit::unlimited(),
            &budget,
            None,
        )?;
        let entry = eval_prog.entry().clone();
        stores.insert(entry.clone(), outcome.store);

        let evaluated: BTreeSet<Tuple> = stores
            .get(&entry)
            .ok_or_else(|| miette::miette!("verify entry relation missing after reinsert"))?
            .all_iter()?
            .map(TupleInIter::try_into_tuple)
            .collect::<Result<BTreeSet<_>, _>>()?;

        let derivation_ceiling = match NonZeroU64::new(
            match options.derived_tuple_ceiling {
                Some(v) => v,
                None => DEFAULT_DERIVED_TUPLE_CEILING,
            }
            .max(1),
        ) {
            Some(n) => n,
            None => miette::bail!("derived_tuple_ceiling max(1) was zero"),
        };
        let unit = NonZeroU64::MIN;
        let weights = |_: &_, _: usize| unit;

        let graph =
            match provenance_graph(&eval_prog, &stores, &budget, derivation_ceiling, &weights) {
                Ok(g) => g,
                Err(e) => {
                    if let Some(lim) = e.downcast_ref::<ProvenanceLimitExceeded>() {
                        return Ok(VerifyOutcome::BudgetRefused {
                            reason: lim.clone(),
                        });
                    }
                    if let Some(u) = e.downcast_ref::<ProvenanceUnsupported>() {
                        return Ok(VerifyOutcome::Unsupported {
                            reason: VerifyUnsupported::ProvenanceUnavailable { reason: u.reason },
                        });
                    }
                    return Err(e);
                }
            };

        let solver_ceiling = match NonZeroU32::new(
            match options.epoch_ceiling {
                Some(v) => v,
                None => DEFAULT_EPOCH_CEILING,
            }
            .max(1),
        ) {
            Some(n) => n,
            None => miette::bail!("epoch_ceiling max(1) was zero"),
        };
        let ann = match solve::<TropicalAnn, _>(&graph, &SolverBudget::new(solver_ceiling)) {
            Ok(a) => a,
            Err(e) => {
                if let Some(lim) = e.downcast_ref::<ProvenanceLimitExceeded>() {
                    return Ok(VerifyOutcome::BudgetRefused {
                        reason: lim.clone(),
                    });
                }
                return Err(e);
            }
        };
        let costs = as_cost_map(&ann);

        let provenance: BTreeSet<Tuple> = costs
            .iter()
            .filter_map(|(node, cost)| match (node, cost) {
                ((PremiseSource::Rule(sym), tup), Cost::Finite(_)) if *sym == entry => {
                    Some(tup.clone())
                }
                _other => None,
            })
            .collect();

        if provenance != evaluated {
            return Ok(VerifyOutcome::Mismatch(Box::new(VerifyMismatch {
                program: mismatch_program,
                evaluated,
                provenance,
                certificate: None,
            })));
        }

        for tup in &evaluated {
            let node = (PremiseSource::Rule(entry.clone()), tup.clone());
            let proof = match extract_min_cost_proof(&graph, &costs, &node) {
                Ok(p) => p,
                Err(e) => {
                    let cert = format!("extract failed for {tup:?}: {e}");
                    return Ok(VerifyOutcome::Mismatch(Box::new(VerifyMismatch {
                        program: mismatch_program,
                        evaluated,
                        provenance,
                        certificate: Some(cert),
                    })));
                }
            };
            if let Err(bad) = verify_proof(&proof, &graph) {
                let cert = format!("verify_proof rejected {tup:?}: {bad}");
                return Ok(VerifyOutcome::Mismatch(Box::new(VerifyMismatch {
                    program: mismatch_program,
                    evaluated,
                    provenance,
                    certificate: Some(cert),
                })));
            }
        }

        Ok(VerifyOutcome::Match {
            row_count: evaluated.len(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::fjall::new_fjall_storage;
    use miette::{Result, miette};

    fn merkle_budget() -> Result<std::num::NonZeroU64> {
        std::num::NonZeroU64::new(1_000_000).ok_or_else(|| miette!("nonzero budget"))
    }

    /// Intact store + lawful chain tip → [`RootVerifyOutcome::Intact`].
    /// A forged [`StateRoot`] sitting in scope is never consulted: `verify`
    /// takes only `(tx, chain, cut, budget)`.
    #[test]
    fn verify_intact_store_matches_stored_root_chain_tip() -> Result<()> {
        use crate::store::epoch::FenceEpoch;
        use crate::store::merkle::{ChainLinkKind, GENESIS_ROOT};
        use crate::store::open::StoreId;
        use crate::store::{Storage, WriteTx};

        let dir = tempfile::tempdir().map_err(|e| miette!("tempdir: {e}"))?;
        let db = new_fjall_storage(dir.path())?;
        let content: Vec<(Vec<u8>, Vec<u8>)> = vec![
            (b"k00".to_vec(), b"v0".to_vec()),
            (b"k01".to_vec(), b"v1".to_vec()),
            (b"k02".to_vec(), b"v2".to_vec()),
        ];
        {
            let mut tx = db.write_tx()?;
            for (k, v) in &content {
                tx.put(k, v)?;
            }
            tx.commit()?;
        }

        let store_id = StoreId::from_digest([0x29; 32]);
        let fence = FenceEpoch::genesis(store_id);
        let cut = CommitOrdinal::ZERO.successor()?;

        let tx = db.read_tx()?;
        let content_root = StateRoot::from_merkle(state_root(&tx, merkle_budget()?)?);

        let mut chain = RootChain::empty();
        assert_eq!(chain.prior_root(), GENESIS_ROOT);
        let link = ChainedStateRoot::mint(
            store_id,
            fence,
            cut,
            content_root,
            chain.prior_root(),
            ChainLinkKind::Ordinary,
        )?;
        chain.append(link)?;

        // A forged digest in scope — verify never takes it as input.
        let _forged_caller_root = StateRoot::from_digest([0xDE; 32]);
        assert_ne!(_forged_caller_root, as_of_root(&chain, cut)?);

        match verify(&tx, &chain, cut, merkle_budget()?).map_err(|e| miette!("verify runs: {e}"))? {
            RootVerifyOutcome::Intact { root } => {
                assert_eq!(root, as_of_root(&chain, cut)?);
                assert!(roots_equal_at_cut(root, chain.prior_root()));
            }
            RootVerifyOutcome::Tampered {
                expected,
                recomputed,
            } => {
                return Err(miette!(
                    "expected Intact, got Tampered {{ expected: {expected:?}, recomputed: {recomputed:?} }}"
                ));
            }
        }
        Ok(())
    }

    /// Real security proof: mutate one stored value after the chain tip is
    /// sealed. Independent rescan + rebind ≠ [`as_of_root`] tip → Tampered.
    /// Cold merkle of the tampered bytes alone is well-formed; detection is
    /// comparison against the stored [`RootChain`], not AEAD or a delivered
    /// digest.
    #[test]
    fn verify_detects_store_tamper_against_stored_root_chain() -> Result<()> {
        use crate::store::epoch::FenceEpoch;
        use crate::store::merkle::ChainLinkKind;
        use crate::store::open::StoreId;
        use crate::store::{Storage, WriteTx};

        let dir = tempfile::tempdir().map_err(|e| miette!("tempdir: {e}"))?;
        let db = new_fjall_storage(dir.path())?;
        {
            let mut tx = db.write_tx()?;
            tx.put(b"k00", b"honest-v0")?;
            tx.put(b"k01", b"honest-v1")?;
            tx.commit()?;
        }

        let store_id = StoreId::from_digest([0xA4; 32]);
        let fence = FenceEpoch::genesis(store_id);
        let cut = CommitOrdinal::ZERO.successor()?;

        let content_root = {
            let tx = db.read_tx()?;
            StateRoot::from_merkle(state_root(&tx, merkle_budget()?)?)
        };

        let mut chain = RootChain::empty();
        let link = ChainedStateRoot::mint(
            store_id,
            fence,
            cut,
            content_root,
            chain.prior_root(),
            ChainLinkKind::Ordinary,
        )?;
        chain.append(link)?;
        let expected_tip = as_of_root(&chain, cut)?;

        // Attacker swaps one value under the sealed tip.
        {
            let mut tx = db.write_tx()?;
            tx.put(b"k00", b"TAMPERED!!")?;
            tx.commit()?;
        }

        let tx = db.read_tx()?;
        let tampered_content = StateRoot::from_merkle(state_root(&tx, merkle_budget()?)?);
        assert_ne!(
            tampered_content, content_root,
            "tamper must change the cold content root"
        );

        match verify(&tx, &chain, cut, merkle_budget()?).map_err(|e| miette!("verify runs: {e}"))? {
            RootVerifyOutcome::Tampered {
                expected,
                recomputed,
            } => {
                assert_eq!(expected, expected_tip);
                assert_ne!(recomputed, expected);
                assert!(!roots_equal_at_cut(expected, recomputed));
            }
            RootVerifyOutcome::Intact { root } => {
                return Err(miette!("tampered store must not Intact; got root={root:?}"));
            }
        }
        Ok(())
    }

    /// Valid-but-stale rollback: an older internally-consistent snapshot
    /// under a tip advanced past that cut. `verify` at the tip cut reports
    /// Tampered — same detection class as merkle's stored-prior test, wired
    /// through the session verify door.
    #[test]
    fn verify_detects_valid_but_stale_rollback_at_tip_cut() -> Result<()> {
        use crate::store::epoch::FenceEpoch;
        use crate::store::merkle::ChainLinkKind;
        use crate::store::open::StoreId;
        use crate::store::{Storage, WriteTx};

        let store_id = StoreId::from_digest([0x58; 32]);
        let fence = FenceEpoch::genesis(store_id);

        let state_v1: Vec<(Vec<u8>, Vec<u8>)> = vec![
            (b"k00".to_vec(), b"v0".to_vec()),
            (b"k01".to_vec(), b"v1".to_vec()),
        ];
        let state_v2: Vec<(Vec<u8>, Vec<u8>)> = {
            let mut s = state_v1.clone();
            s.push((b"k02".to_vec(), b"v2".to_vec()));
            s
        };
        let state_v3: Vec<(Vec<u8>, Vec<u8>)> = {
            let mut s = state_v2.clone();
            s.push((b"k03".to_vec(), b"v3".to_vec()));
            s
        };

        fn write_state(pairs: &[(Vec<u8>, Vec<u8>)]) -> Result<crate::store::fjall::FjallStorage> {
            let dir = tempfile::tempdir().map_err(|e| miette!("tempdir: {e}"))?;
            let db = new_fjall_storage(dir.path())?;
            std::mem::forget(dir);
            let mut tx = db.write_tx()?;
            for (k, v) in pairs {
                tx.put(k, v)?;
            }
            tx.commit()?;
            Ok(db)
        }

        let db_v1 = write_state(&state_v1)?;
        let content_v1 = StateRoot::from_merkle(state_root(&db_v1.read_tx()?, merkle_budget()?)?);
        let db_v2 = write_state(&state_v2)?;
        let content_v2 = StateRoot::from_merkle(state_root(&db_v2.read_tx()?, merkle_budget()?)?);
        let db_v3 = write_state(&state_v3)?;
        let content_v3 = StateRoot::from_merkle(state_root(&db_v3.read_tx()?, merkle_budget()?)?);
        assert_ne!(content_v1, content_v2);
        assert_ne!(content_v2, content_v3);

        let o1 = CommitOrdinal::ZERO.successor()?;
        let o2 = o1.successor()?;
        let o3 = o2.successor()?;

        let mut chain = RootChain::empty();
        chain.append(ChainedStateRoot::mint(
            store_id,
            fence,
            o1,
            content_v1,
            chain.prior_root(),
            ChainLinkKind::Ordinary,
        )?)?;
        chain.append(ChainedStateRoot::mint(
            store_id,
            fence,
            o2,
            content_v2,
            chain.prior_root(),
            ChainLinkKind::Ordinary,
        )?)?;
        chain.append(ChainedStateRoot::mint(
            store_id,
            fence,
            o3,
            content_v3,
            chain.prior_root(),
            ChainLinkKind::Ordinary,
        )?)?;

        // Live tip store matches tip cut.
        assert!(matches!(
            verify(&db_v3.read_tx()?, &chain, o3, merkle_budget()?)?,
            RootVerifyOutcome::Intact { .. }
        ));

        // Attacker restores an older internally-consistent backup (v1 bytes).
        let rolled = write_state(&state_v1)?;
        let rolled_content =
            StateRoot::from_merkle(state_root(&rolled.read_tx()?, merkle_budget()?)?);
        assert_eq!(
            rolled_content, content_v1,
            "rolled-back store must be internally consistent with v1"
        );

        match verify(&rolled.read_tx()?, &chain, o3, merkle_budget()?)? {
            RootVerifyOutcome::Tampered {
                expected,
                recomputed,
            } => {
                assert_eq!(expected, as_of_root(&chain, o3)?);
                assert!(!roots_equal_at_cut(expected, recomputed));
            }
            RootVerifyOutcome::Intact { .. } => {
                return Err(miette!("valid-but-stale rollback at tip must Tamper"));
            }
        }

        // Same rolled-back bytes still Intact at the older cut they match.
        match verify(&rolled.read_tx()?, &chain, o1, merkle_budget()?)? {
            RootVerifyOutcome::Intact { root } => {
                assert_eq!(root, as_of_root(&chain, o1)?);
            }
            RootVerifyOutcome::Tampered { .. } => {
                return Err(miette!("v1 bytes must Intact against as-of cut o1"));
            }
        }
        Ok(())
    }

    #[test]
    fn provenance_verify_matches_transitive_closure() -> Result<()> {
        use crate::session::catalog::Catalog;
        let dir = tempfile::tempdir().map_err(|e| miette!("tempdir: {e}"))?;
        let storage = new_fjall_storage(dir.path())?;
        let db = Engine::compose(storage, Catalog::new()).map_err(|e| miette!("compose: {e}"))?;
        db.run_script(":create edge {a: Int, b: Int}", Default::default())
            .map_err(|e| miette!("create schema: {e}"))?;
        let rows = DataValue::List(vec![
            DataValue::List(vec![DataValue::from(1i64), DataValue::from(2i64)]),
            DataValue::List(vec![DataValue::from(2i64), DataValue::from(3i64)]),
            DataValue::List(vec![DataValue::from(3i64), DataValue::from(4i64)]),
        ]);
        db.run_script(
            "?[a, b] <- $rows :put edge {a, b}",
            BTreeMap::from([("rows".into(), rows)]),
        )
        .map_err(|e| miette!("seed: {e}"))?;

        let outcome = db
            .verify_script(
                r#"
                path[x, y] := *edge[x, y]
                path[x, z] := path[x, y], *edge[y, z]
                ?[x, y] := path[x, y]
                "#,
                Default::default(),
                ScriptOptions::new(),
            )
            .map_err(|e| miette!("verify_script runs: {e}"))?;
        match outcome {
            VerifyOutcome::Match { row_count } => assert_eq!(row_count, 6),
            other @ VerifyOutcome::Mismatch(_)
            | other @ VerifyOutcome::Unsupported { .. }
            | other @ VerifyOutcome::BudgetRefused { .. } => {
                return Err(miette!("expected Match, got {other:?}"));
            }
        }
        Ok(())
    }

    #[test]
    fn provenance_verify_directive_returns_match_row() -> Result<()> {
        use crate::session::catalog::Catalog;
        let dir = tempfile::tempdir().map_err(|e| miette!("tempdir: {e}"))?;
        let storage = new_fjall_storage(dir.path())?;
        let db = Engine::compose(storage, Catalog::new()).map_err(|e| miette!("compose: {e}"))?;
        db.run_script(":create edge {a: Int, b: Int}", Default::default())
            .map_err(|e| miette!("create schema: {e}"))?;
        let rows = DataValue::List(vec![
            DataValue::List(vec![DataValue::from(1i64), DataValue::from(2i64)]),
            DataValue::List(vec![DataValue::from(2i64), DataValue::from(3i64)]),
        ]);
        db.run_script(
            "?[a, b] <- $rows :put edge {a, b}",
            BTreeMap::from([("rows".into(), rows)]),
        )
        .map_err(|e| miette!("seed: {e}"))?;

        let rows = db
            .run_script(
                r#"
                ::verify {
                    path[x, y] := *edge[x, y]
                    path[x, z] := path[x, y], *edge[y, z]
                    ?[x, y] := path[x, y]
                }
                "#,
                Default::default(),
            )
            .map_err(|e| miette!("::verify runs: {e}"))?;
        assert_eq!(rows.headers(), &["status", "summary", "detail"]);
        assert_eq!(rows.rows().len(), 1);
        assert_eq!(rows.rows()[0][0], DataValue::from("match"));
        Ok(())
    }

    /// Seeded edge relation for refuse-path pins through [`Engine::verify_script`].
    fn seeded_edge_db() -> Result<Engine<crate::store::fjall::FjallStorage>> {
        use crate::session::catalog::Catalog;
        let dir = tempfile::tempdir().map_err(|e| miette!("tempdir: {e}"))?;
        let storage = new_fjall_storage(dir.path())?;
        std::mem::forget(dir);
        let db = Engine::compose(storage, Catalog::new()).map_err(|e| miette!("compose: {e}"))?;
        db.run_script(":create edge {a: Int, b: Int}", Default::default())
            .map_err(|e| miette!("create schema: {e}"))?;
        let rows = DataValue::List(vec![
            DataValue::List(vec![DataValue::from(1i64), DataValue::from(2i64)]),
            DataValue::List(vec![DataValue::from(2i64), DataValue::from(3i64)]),
            DataValue::List(vec![DataValue::from(3i64), DataValue::from(4i64)]),
        ]);
        db.run_script(
            "?[a, b] <- $rows :put edge {a, b}",
            BTreeMap::from([("rows".into(), rows)]),
        )
        .map_err(|e| miette!("seed: {e}"))?;
        Ok(db)
    }

    /// `:put` reaches [`verify_input_program`] → [`VerifyUnsupported::Mutation`].
    /// Hand-constructed `Unsupported { Mutation }` never proved this door.
    #[test]
    fn verify_input_program_refuses_mutation() -> Result<()> {
        let db = seeded_edge_db()?;
        let outcome = db
            .verify_script(
                "?[a, b] := *edge[a, b] :put edge {a, b}",
                Default::default(),
                ScriptOptions::new(),
            )
            .map_err(|e| miette!("verify_script returns outcome, not Err: {e}"))?;
        assert!(
            matches!(
                outcome,
                VerifyOutcome::Unsupported {
                    reason: VerifyUnsupported::Mutation
                }
            ),
            "expected Mutation refuse, got {outcome:?}"
        );
        Ok(())
    }

    /// `:order` reaches [`verify_input_program`] → [`VerifyUnsupported::OrderLimitOffset`].
    #[test]
    fn verify_input_program_refuses_order_limit_offset() -> Result<()> {
        let db = seeded_edge_db()?;
        let outcome = db
            .verify_script(
                "?[a, b] := *edge[a, b] :order a",
                Default::default(),
                ScriptOptions::new(),
            )
            .map_err(|e| miette!("verify_script returns outcome, not Err: {e}"))?;
        assert!(
            matches!(
                outcome,
                VerifyOutcome::Unsupported {
                    reason: VerifyUnsupported::OrderLimitOffset
                }
            ),
            "expected OrderLimitOffset refuse, got {outcome:?}"
        );
        Ok(())
    }

    /// `@spans` reaches [`verify_input_program`] → [`VerifyUnsupported::IntervalDerivation`].
    #[test]
    fn verify_input_program_refuses_interval_derivation() -> Result<()> {
        let db = seeded_edge_db()?;
        db.run_script(":create hist {k: Int => v: Any}", Default::default())
            .map_err(|e| miette!("create hist: {e}"))?;
        let outcome = db
            .verify_script(
                "?[k, v, iv] := *hist[k, v @spans iv]",
                Default::default(),
                ScriptOptions::new(),
            )
            .map_err(|e| miette!("verify_script returns outcome, not Err: {e}"))?;
        match outcome {
            VerifyOutcome::Unsupported {
                reason: VerifyUnsupported::IntervalDerivation { name },
            } => assert_eq!(name, "hist"),
            other @ (VerifyOutcome::Match { .. }
            | VerifyOutcome::Mismatch(_)
            | VerifyOutcome::BudgetRefused { .. }
            | VerifyOutcome::Unsupported { .. }) => {
                return Err(miette!(
                    "expected IntervalDerivation {{ hist }}, got {other:?}"
                ));
            }
        }
        Ok(())
    }

    /// Starved provenance ceiling: eval completes, provenance enumeration
    /// refuses → [`VerifyOutcome::BudgetRefused`] (not Err, not Match).
    #[test]
    fn verify_input_program_refuses_budget() -> Result<()> {
        use crate::session::catalog::Catalog;
        let dir = tempfile::tempdir().map_err(|e| miette!("tempdir: {e}"))?;
        let storage = new_fjall_storage(dir.path())?;
        std::mem::forget(dir);
        let db = Engine::compose(storage, Catalog::new()).map_err(|e| miette!("compose: {e}"))?;
        db.run_script(":create edge {a: Int, b: Int}", Default::default())
            .map_err(|e| miette!("create edge: {e}"))?;
        let mut pairs = Vec::new();
        let layers = [0i64, 4, 8, 12, 16];
        for w in layers.windows(2) {
            let (a0, a1) = (w[0], w[1]);
            for i in a0..a1 {
                for j in a1..(a1 + (a1 - a0)) {
                    if j <= 19 {
                        pairs.push(DataValue::List(vec![
                            DataValue::from(i),
                            DataValue::from(j),
                        ]));
                    }
                }
            }
        }
        for i in 0..12 {
            for j in (i + 1)..12 {
                pairs.push(DataValue::List(vec![
                    DataValue::from(i),
                    DataValue::from(j),
                ]));
            }
        }
        db.run_script(
            "?[a, b] <- $rows :put edge {a, b}",
            BTreeMap::from([("rows".into(), DataValue::List(pairs))]),
        )
        .map_err(|e| miette!("seed dense edge: {e}"))?;

        let outcome = db
            .verify_script(
                r#"
                path[x, y] := *edge[x, y]
                path[x, z] := path[x, y], path[y, z]
                ?[x, y] := path[x, y]
                "#,
                Default::default(),
                ScriptOptions {
                    derived_tuple_ceiling: Some(500),
                    epoch_ceiling: Some(1_000_000),
                    ..ScriptOptions::new()
                },
            )
            .map_err(|e| miette!("starved ceiling returns BudgetRefused, not Err: {e}"))?;
        assert!(
            matches!(outcome, VerifyOutcome::BudgetRefused { .. }),
            "expected BudgetRefused, got {outcome:?}"
        );
        Ok(())
    }
}
