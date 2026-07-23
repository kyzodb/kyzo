/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Story #61's standing-query lifecycle: registration, snapshot-consistent
//! initialization, patch application, and teardown — built on
//! [`crate::react::incremental`]'s translator and evaluator, and on
//! `runtime::callback`'s existing per-relation commit-notification seam.
//!
//! ## Snapshot consistency, for free
//!
//! [`Db::current_callback_targets`] is read exactly ONCE per transaction
//! (`runtime/callback.rs`'s own doc: "a registration racing a commit
//! either sees all of it or none of it"). [`StandingQuery::register`]
//! exploits this directly: it registers a callback on every EDB relation
//! the translated program depends on FIRST, then reads each relation's
//! CURRENT rows SECOND. A commit landing in between either predates
//! registration (already reflected in the initial read; the callback
//! event that eventually arrives for it is redundant) or postdates it
//! (missed by the initial read; the callback event supplies it) — there
//! is no window where a commit is lost. The redundant case needs no new
//! locking: [`incremental::incremental_eval`]'s own EDB-patch filtering
//! (a `Plus` for an already-present fact, or a `Minus` for an absent one,
//! is dropped as a no-op) already absorbs it.
//!
//! ## The drive model: pull, not a background thread
//!
//! `Db::register_callback` already returns a plain
//! `std::sync::mpsc::Receiver` — there is no thread-management
//! infrastructure anywhere in the callback seam today, and inventing one
//! (shutdown coordination, a panic inside a spawned thread) is a
//! separate, larger concern this story does not need to solve.
//! [`StandingQuery::apply_pending`] drains whatever is currently queued
//! across every subscribed relation's receiver and applies it in one
//! pass; the caller decides whether that means a poll loop, driving it
//! once per query, or a thread of their own.
//!
//! ## `CallbackEvent` → `SignedFact`
//!
//! `CallbackOp::Put`'s `new`/`old` `NamedRows` are two INDEPENDENT row
//! sets sharing the relation's full key+value header (never index-paired
//! — `runtime/mutate.rs`'s `collect_mutations` builds them as separate
//! lists, not a zipped diff). They are NOT disjoint, though: re-putting a
//! key with unchanged content reports that same full row on BOTH sides
//! (it "replaced" itself) — the real-commit differential
//! (`incremental_matches_recompute_across_real_commit_sequences`) caught
//! this the first time it ran a redundant `:put` (a `Plus`+`Minus` pair
//! for the identical row reaching `incremental_eval` as raw facts, where
//! the `Plus` gets filtered as redundant but the lone surviving `Minus`
//! then wrongly retracted a fact nothing removed). The fix takes the SET
//! DIFFERENCE before folding anything into the patch: only a row in `new`
//! but not `old` is a `Plus`, only a row in `old` but not `new` is a
//! `Minus` — a row on both sides nets to no change, exactly as it should.
//! `CallbackOp::Rm` is NOT symmetric: its "new" side is built from the
//! relation's KEY-ONLY columns (`k_bindings`, not `kv_bindings`) — a bare
//! key, not a real row this program's arity would match — so an `Rm`
//! event contributes `Minus` from "old" only; "new" is never a fact.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc::Receiver;

use miette::{Diagnostic, Result, ensure, miette};
use thiserror::Error;

use crate::exec::op::temporal::SignedFact;
use crate::parse::{Script, parse_script};
use crate::react::incremental::{self, IncrementalProgram, MaintainedState};
use crate::rules::contract::CancelFlag;
use crate::session::catalog::get_relation;
use crate::session::current_validity;
use crate::session::db::{Engine, ScriptOptions, SessionTx};
use crate::session::db::{SessionNormalizer, SessionView};
use crate::session::observe::{CallbackEvent, CallbackId, CallbackOp};
use crate::store::Storage;
use kyzo_model::SourceSpan;
use kyzo_model::program::symbol::Symbol;
use kyzo_model::value::DataValue;
use kyzo_model::value::Tuple;

/// Named refusal when [`Db::register_standing`] is given a non-standing script.
#[cfg(test)]
use crate::session::catalog::Catalog;
#[derive(Debug, Error, Diagnostic)]
pub(crate) enum StandingRegisterRefusal {
    #[error(
        "register_standing needs a single read query, not a system op or an \
         imperative script"
    )]
    #[diagnostic(code(standing::not_single_read))]
    NotSingleRead,
    #[error(
        "register_standing needs a pure read query, not a mutation — a standing query \
         maintains its OWN state from EDB commits, it does not write one"
    )]
    #[diagnostic(code(standing::mutation))]
    Mutation,
}

/// KyzoScript's own canonical name for the entry relation — the same
/// identity `InputProgram::entry_name` carries and this module's
/// translator preserves through `MagicSymbol`'s unadorned rendering
/// (entry rules are never magic-set adorned, since nothing "demands"
/// them). Minted fresh per call rather than cached: cheap (one short
/// `SmartString`), and it keeps `Symbol` construction in exactly one
/// place instead of `Symbol::new("?", …)` recurring at every call site.
fn entry_symbol() -> Symbol {
    Symbol::new("?", SourceSpan::empty())
}

/// One EDB dependency's live subscription: the callback id (for
/// [`StandingQuery::teardown`]) and the receiver it delivers on.
struct Subscription {
    id: CallbackId,
    receiver: Receiver<CallbackEvent>,
    /// This relation's key-column count (`StoredRelationMetadata::keys.len()`
    /// at registration time), cached so [`StandingQuery::apply_pending`]
    /// never needs a second storage round-trip just to re-derive it — used
    /// only by the debug-only duplicate-key invariant check below.
    key_arity: usize,
}

/// Debug-only invariant: a key-valued relation's maintained row set never
/// holds two rows sharing the same key-column prefix — the exact structural
/// violation the multi-commit-drain bug (0.9.0 adversarial review, this
/// module's own repro 3 above) produced. `Tuple`'s `Ord` is lexicographic by
/// column position with key columns first (the storage layer's own
/// convention: a `RelationHandle`'s columns are `keys` then `non_keys`, in
/// that order), so two rows sharing a key prefix are always ADJACENT in a
/// `BTreeSet<Tuple>`'s iteration order — an O(n) adjacent-pair scan suffices,
/// no need to build a separate key-only index. `key_arity` of `0` means
/// every row IS the key: trivially duplicate-free once `BTreeSet` has
/// already deduplicated identical rows.
fn no_duplicate_key_prefix(rows: &BTreeSet<Tuple>, key_arity: usize) -> bool {
    if key_arity == 0 {
        return true;
    }
    let mut prev: Option<&Tuple> = None;
    for row in rows {
        if let Some(p) = prev
            && p.len() >= key_arity
            && row.len() >= key_arity
            && p.as_slice()[..key_arity] == row.as_slice()[..key_arity]
        {
            return false;
        }
        prev = Some(row);
    }
    true
}

/// A live standing query: a translated program, its persistently
/// maintained state, and one subscription per EDB relation it depends
/// on. Construction (via [`Db::register_standing`], the public entry
/// point, or [`StandingQuery::register`] for an already-compiled
/// program) is the only way to get one — there is no bare-fields
/// constructor, so a `StandingQuery` that exists is always both
/// subscribed and snapshot-initialized.
pub struct StandingQuery<S: Storage> {
    db: Engine<S>,
    program: IncrementalProgram,
    state: MaintainedState,
    subscriptions: BTreeMap<Symbol, Subscription>,
}

impl<S: Storage> StandingQuery<S> {
    /// Register a standing query from an already magic-set-rewritten
    /// compiled program: translate it, then subscribe to and snapshot
    /// every EDB relation it depends on, in that order (the
    /// snapshot-consistency argument in the module doc depends on this
    /// order — subscribe first, read second). [`Db::register_standing`]
    /// is the public entry point most callers want; this is its
    /// internal-facing half, exposed for callers that already hold a
    /// compiled [`StratifiedMagicProgram`](crate::exec::plan::program::StratifiedMagicProgram).
    pub(crate) fn register(
        db: &Engine<S>,
        magic: crate::exec::plan::program::StratifiedMagicProgram,
    ) -> Result<Self> {
        let program = incremental::translate(magic)?;
        let edb = incremental::edb_relations_pub(&program);

        let mut subscriptions = BTreeMap::new();
        for rel in &edb {
            let (id, receiver) = db.register_callback(rel.name.as_str())?;
            // `key_arity` is filled in below, once the same relation's
            // handle is fetched to snapshot its rows — a placeholder here
            // would just be the same lookup done twice.
            subscriptions.insert(
                rel.clone(),
                Subscription {
                    id,
                    receiver,
                    key_arity: 0,
                },
            );
        }

        let tx = db.store.read_tx()?;
        let mut state: MaintainedState = BTreeMap::new();
        for rel in &edb {
            let handle = get_relation(&tx, rel.name.as_str())?;
            let key_arity = handle.metadata.keys.len();
            let rows: BTreeSet<Tuple> = handle.scan_all(&tx).collect::<Result<_>>()?;
            ensure!(
                no_duplicate_key_prefix(&rows, key_arity),
                "relation {rel:?}'s freshly-scanned rows already have a duplicate key at \
                 registration time (key_arity {key_arity}): {rows:?}"
            );
            subscriptions
                .get_mut(rel)
                .ok_or_else(|| miette!("standing subscription missing after insert"))?
                .key_arity = key_arity;
            state.insert(rel.clone(), rows);
        }
        drop(tx);

        // The very first evaluation is a full recompute (there is no
        // "before" patch yet) — the derived relations' own state starts
        // empty and `incremental_eval` with an all-EDB, all-`Plus` patch
        // derives them in one pass, the same topological walk every
        // later patch uses. This is NOT a special case bolted on top: an
        // empty `MaintainedState` for every IDB relation is exactly the
        // correct "nothing derived yet" starting point.
        let seed_patch: BTreeMap<Symbol, BTreeSet<SignedFact>> = state
            .iter()
            .map(|(rel, rows)| {
                let patch: BTreeSet<SignedFact> =
                    rows.iter().map(|t| SignedFact::Plus(t.clone())).collect();
                (rel.clone(), patch)
            })
            .collect();
        let edb_only_state: MaintainedState = state
            .keys()
            .map(|rel| (rel.clone(), BTreeSet::new()))
            .collect();
        let (_deltas, full_state) =
            incremental::incremental_eval(&program, &edb_only_state, &seed_patch)?;

        Ok(StandingQuery {
            db: db.clone(),
            program,
            state: full_state,
            subscriptions,
        })
    }

    /// The standing query's current answer set for `rel` (its own head,
    /// or any relation in its dependency chain) — `None` if `rel` is not
    /// part of this program at all.
    pub fn current(&self, rel: &Symbol) -> Option<&BTreeSet<Tuple>> {
        self.state.get(rel)
    }

    /// The standing query's OWN answer set — the entry relation (`?`,
    /// KyzoScript's own canonical name for it, the same identity
    /// `InputProgram`'s `entry_name` field carries and this module's
    /// translator preserves through `MagicSymbol`'s unadorned rendering)
    /// — without the caller ever needing to construct a `Symbol` just to
    /// read back the one relation almost every caller wants. `current`
    /// is still there for a caller that knows a specific intermediate
    /// dependency's own `Symbol` and wants ITS state instead.
    pub fn current_answer(&self) -> &BTreeSet<Tuple> {
        static EMPTY: BTreeSet<Tuple> = BTreeSet::new();
        match self.current(&entry_symbol()) {
            Some(rows) => rows,
            None => &EMPTY,
        }
    }

    /// Drain every subscribed relation's pending callback events, fold
    /// them into one signed EDB patch, and apply it — returning the
    /// signed delta every relation (EDB and IDB alike) underwent. An
    /// empty return means nothing was pending.
    pub fn apply_pending(&mut self) -> Result<BTreeMap<Symbol, BTreeSet<SignedFact>>> {
        let mut edb_patch: BTreeMap<Symbol, BTreeSet<SignedFact>> = BTreeMap::new();
        for (rel, sub) in &self.subscriptions {
            // Net each tuple's signed multiplicity IN COMMIT ORDER
            // (`try_recv`'s own FIFO order) before this relation's patch
            // ever reaches `incremental_eval` — draining more than one
            // queued commit's worth of events in a single pass (the
            // whole point of a pull-based drive model: poll less often
            // than every commit) means the SAME tuple can appear as
            // both a `Plus` and a `Minus` across DIFFERENT events
            // (assert-then-retract, retract-then-reassert, or two puts
            // of DIFFERENT values at the SAME key). A flat
            // `BTreeSet<SignedFact>` cannot represent that cancellation
            // — `Plus(t)` and `Minus(t)` are distinct set elements, so
            // both survive into the patch — and `incremental_eval`'s
            // redundancy filter, which only ever sees the state from
            // BEFORE this whole batch, has no way to recover it:
            // whichever side happens to already match pre-batch state
            // survives, the other is silently dropped. In the
            // key-value-relation case that means two puts of the same
            // key can leave BOTH values in the maintained state at
            // once — a structural key-uniqueness violation that
            // poisons every downstream rule (0.9.0 adversarial review,
            // confirmed; see this module's own repro test). Netting
            // first turns whatever `incremental_eval` receives back
            // into an actually well-formed single patch, which its
            // filter already handles correctly.
            let mut net: BTreeMap<Tuple, i64> = BTreeMap::new();
            while let Ok(event) = sub.receiver.try_recv() {
                match event.op {
                    CallbackOp::Put => {
                        // A row present in BOTH sets is a redundant re-put
                        // of identical content at the same key (no value
                        // columns changed, or a key-only relation like the
                        // test fixtures below) — net effect is no change,
                        // not a Minus-then-filtered-Plus pair. Computing
                        // the difference here, not raw per-set facts, is
                        // what keeps that case from ever contributing a
                        // spurious `Minus`.
                        let new_set: BTreeSet<Tuple> = event.new_rows.into_iter().collect();
                        let old_set: BTreeSet<Tuple> = event.old_rows.into_iter().collect();
                        for row in new_set.difference(&old_set) {
                            *net.entry(row.clone()).or_default() += 1;
                        }
                        for row in old_set.difference(&new_set) {
                            *net.entry(row.clone()).or_default() -= 1;
                        }
                    }
                    CallbackOp::Rm => {
                        // `new` here is bare keys (k_bindings), never a
                        // real row this program's arity matches — only
                        // `old` (the full removed row) is a fact.
                        for row in event.old_rows {
                            *net.entry(row).or_default() -= 1;
                        }
                    }
                }
            }
            let entry = edb_patch.entry(rel.clone()).or_default();
            for (row, n) in net {
                match n.cmp(&0) {
                    std::cmp::Ordering::Greater => {
                        entry.insert(SignedFact::Plus(row));
                    }
                    std::cmp::Ordering::Less => {
                        entry.insert(SignedFact::Minus(row));
                    }
                    // A net-zero tuple never had a real effect across
                    // this whole batch (assert-then-retract, or the
                    // reverse) — dropped here, not merely filtered
                    // later, so it never even looks like a candidate
                    // patch entry.
                    std::cmp::Ordering::Equal => {}
                }
            }
        }
        if edb_patch.values().all(BTreeSet::is_empty) {
            return Ok(BTreeMap::new());
        }
        let (deltas, new_state) =
            incremental::incremental_eval(&self.program, &self.state, &edb_patch)?;
        ensure!(
            self.subscriptions.iter().all(|(rel, sub)| {
                new_state
                    .get(rel)
                    .is_none_or(|rows| no_duplicate_key_prefix(rows, sub.key_arity))
            }),
            "apply_pending produced a duplicate key in a maintained EDB relation's row set — \
             the exact structural violation the netting step above exists to prevent"
        );
        self.state = new_state;
        Ok(deltas)
    }

    /// [`apply_pending`](Self::apply_pending), returning only the entry
    /// relation's own signed delta — the `Symbol`-free counterpart of
    /// [`current_answer`](Self::current_answer), for a caller that only
    /// cares about the query's own answer changing, not an intermediate
    /// dependency's.
    pub fn apply_pending_answer(&mut self) -> Result<BTreeSet<SignedFact>> {
        Ok(match self.apply_pending()?.remove(&entry_symbol()) {
            Some(delta) => delta,
            None => BTreeSet::new(),
        })
    }

    /// Explicitly tear down NOW — unregister every underlying per-relation
    /// callback — instead of waiting for the [`Drop`] on scope exit that
    /// guarantees the identical release. A `StandingQuery` is a `Drop` type,
    /// so the unregistration lives in that impl; this is simply an eager,
    /// explicit drop. A query dropped without calling this releases exactly
    /// the same way — RAII makes leaking a live registration by forgetting
    /// impossible.
    pub fn teardown(self) {
        drop(self);
    }
}

/// RAII: a `StandingQuery`'s callback subscriptions are an acquired resource,
/// released on scope exit. Every underlying per-relation callback is
/// unregistered when the query is dropped, whether or not
/// [`teardown`](StandingQuery::teardown) was called — so a forgotten teardown
/// can no longer leak a live registration (the registry's lossy-by-disconnect
/// pruning is now the second line of defense, not the first). Making this a
/// `Drop` impl is what forces the unregistration to live in exactly one place
/// rather than a consuming method a caller can skip: a `Drop` type cannot
/// move its fields out, so `teardown` can only delegate here.
/// Drop cannot refuse: a poisoned observe registry surfaces as
/// [`crate::session::observe::ObserveRefuse`] and is discarded here rather
/// than panicking on unwind.
impl<S: Storage> Drop for StandingQuery<S> {
    fn drop(&mut self) {
        for sub in self.subscriptions.values() {
            // Drop cannot refuse: ObserveRefuse on a poisoned registry is named and discarded.
            let _unregister_outcome = self.db.unregister_callback(sub.id);
        }
    }
}

impl<S: Storage> Engine<S> {
    /// Register a standing query from a real KyzoScript read query — the
    /// public entry point: a user hands this a query string, exactly as
    /// they would to [`Db::run_script`], and gets back a live
    /// [`StandingQuery`] whose [`apply_pending`](StandingQuery::apply_pending)
    /// stays correct across every commit from here on.
    ///
    /// Runs the SAME prefix `compile_and_eval` runs for an ordinary read
    /// query — parse, normalize, stratify, magic-sets rewrite — over one
    /// shared read snapshot, then stops there and hands the result to
    /// [`StandingQuery::register`] instead of continuing on to
    /// `stratified_magic_compile`'s RA lowering (which is exactly the
    /// erasure this story's translator cannot afford — see
    /// `react::incremental`'s module doc on why `MagicAtom`, not
    /// `RelAlgebra`, is the translation source).
    pub fn register_standing(
        &self,
        query: &str,
        params: BTreeMap<String, DataValue>,
    ) -> Result<StandingQuery<S>> {
        let cur_vld = current_validity()?;
        let program = match parse_script(query, &params, cur_vld)? {
            Script::Query(prog) => prog,
            Script::Sys(_) | Script::Imperative(_) => {
                return Err(StandingRegisterRefusal::NotSingleRead.into());
            }
        };
        if program.out_opts().store_relation.is_some() {
            return Err(StandingRegisterRefusal::Mutation.into());
        }

        let tx = SessionTx::new_read(self.store.read_tx()?, ScriptOptions::new());
        let view = SessionView {
            store: &tx.store,
            temp: &tx.temp,
        };
        let cancel = CancelFlag::inert();
        let mut normalizer = SessionNormalizer::new(view, cancel);
        let (nf, _out_opts) =
            crate::exec::plan::program::into_normalized_program(program, &mut normalizer)?;
        let (strat, _lifetimes) = nf.into_stratified_program()?;
        let magic = strat.magic_sets_rewrite(&view)?;

        StandingQuery::register(self, magic)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::plan::program::{
        MagicAtom, MagicInlineRule, MagicProgram, MagicRelationApplyAtom, MagicRulesOrFixed,
        MagicSymbol, StratifiedMagicProgram,
    };
    use crate::store::fjall::new_fjall_storage;
    use kyzo_model::program::rule::HeadAggrSlot;
    use kyzo_model::value::{DataValue, Num};
    use miette::{Result, miette};

    fn sym(name: &str) -> Symbol {
        Symbol::new(name, SourceSpan::empty())
    }
    fn v(i: i64) -> DataValue {
        DataValue::Num(Num::int(i))
    }
    fn no_params() -> BTreeMap<String, DataValue> {
        BTreeMap::new()
    }
    /// Absent relation key → empty row set (not yet touched).
    fn current_rows<S: Storage>(sq: &StandingQuery<S>, rel: &Symbol) -> BTreeSet<Tuple> {
        match sq.current(rel) {
            Some(rows) => rows.clone(),
            None => BTreeSet::new(),
        }
    }
    /// Absent delta key → empty signed set (relation unchanged this round).
    fn delta_for(
        deltas: &BTreeMap<Symbol, BTreeSet<SignedFact>>,
        rel: &Symbol,
    ) -> BTreeSet<SignedFact> {
        match deltas.get(rel) {
            Some(d) => d.clone(),
            None => BTreeSet::new(),
        }
    }

    fn rel_atom(name: &str, args: Vec<&str>, negated: bool) -> MagicAtom {
        let atom = MagicRelationApplyAtom {
            name: sym(name),
            args: args.into_iter().map(sym).collect(),
            validity: None,
            span: SourceSpan::empty(),
        };
        if negated {
            MagicAtom::NegatedRelation(atom)
        } else {
            MagicAtom::Relation(atom)
        }
    }
    fn magic_inline(head: Vec<&str>, body: Vec<MagicAtom>) -> MagicInlineRule {
        let aggr = (0..head.len()).map(|_| HeadAggrSlot::Plain).collect();
        MagicInlineRule {
            head: head.into_iter().map(sym).collect(),
            aggr,
            body,
        }
    }
    fn one_stratum_program(
        defs: Vec<(&str, Vec<MagicInlineRule>)>,
    ) -> Result<StratifiedMagicProgram> {
        let prog = defs
            .into_iter()
            .map(|(head, rules)| {
                (
                    MagicSymbol::Muggle { inner: sym(head) },
                    MagicRulesOrFixed::Rules { rules },
                )
            })
            .collect();
        StratifiedMagicProgram::from_execution_order(vec![MagicProgram { prog }])
    }

    /// `?(x) :- p(x), not r(x)` — the hard-corner scenario, from a REAL
    /// Db/callback registration through to a real commit's delta.
    fn hard_corner_program() -> Result<StratifiedMagicProgram> {
        one_stratum_program(vec![(
            "?",
            vec![magic_inline(
                vec!["X"],
                vec![
                    rel_atom("p", vec!["X"], false),
                    rel_atom("r", vec!["X"], true),
                ],
            )],
        )])
    }

    #[test]
    fn register_snapshots_current_state_then_apply_pending_tracks_real_commits() -> Result<()> {
        let dir = tempfile::tempdir().map_err(|e| miette!("{e}"))?;
        let db = Engine::compose(new_fjall_storage(dir.path())?, Catalog::new())?;
        db.run_script(":create p {x: Int =>}", no_params())?;
        db.run_script(":create r {x: Int =>}", no_params())?;
        // p(1) exists, r is empty: q(1) should already hold at registration.
        db.run_script("?[x] <- [[1]] :put p {x}", no_params())?;

        let mut sq = StandingQuery::register(&db, hard_corner_program()?)?;
        assert_eq!(
            current_rows(&sq, &sym("?")),
            [vec![v(1)]].into_iter().map(Tuple::from_vec).collect(),
            "q(1) must already hold from the pre-registration snapshot"
        );

        // Nothing committed since registration: apply_pending is a no-op.
        assert!(sq.apply_pending()?.is_empty());

        // The hard corner: retract r's ABSENCE by asserting into it —
        // q(1) must be retracted.
        db.run_script("?[x] <- [[1]] :put r {x}", no_params())?;
        let deltas = sq.apply_pending()?;
        assert_eq!(
            delta_for(&deltas, &sym("?")),
            [SignedFact::Minus(Tuple::from_vec(vec![v(1)]))]
                .into_iter()
                .collect()
        );
        assert!(
            sq.current(&sym("?"))
                .ok_or_else(|| miette!("current"))?
                .is_empty()
        );

        // The mirror: retracting r(1) makes q(1) hold again.
        db.run_script("?[x] <- [[1]] :rm r {x}", no_params())?;
        let deltas = sq.apply_pending()?;
        assert_eq!(
            delta_for(&deltas, &sym("?")),
            [SignedFact::Plus(Tuple::from_vec(vec![v(1)]))]
                .into_iter()
                .collect()
        );
        assert_eq!(
            current_rows(&sq, &sym("?")),
            [vec![v(1)]].into_iter().map(Tuple::from_vec).collect()
        );

        sq.teardown();
        Ok(())
    }

    /// `current_answer`/`apply_pending_answer` — the `Symbol`-free
    /// entry-relation accessors an HTTP handler or any other caller
    /// without a `Symbol` in hand needs — agree exactly with the
    /// `Symbol`-keyed calls at every step.
    #[test]
    fn current_answer_and_apply_pending_answer_match_the_symbol_keyed_calls() -> Result<()> {
        let dir = tempfile::tempdir().map_err(|e| miette!("{e}"))?;
        let db = Engine::compose(new_fjall_storage(dir.path())?, Catalog::new())?;
        db.run_script(":create p {x: Int =>}", no_params())?;
        db.run_script(":create r {x: Int =>}", no_params())?;
        db.run_script("?[x] <- [[1]] :put p {x}", no_params())?;

        let mut sq = StandingQuery::register(&db, hard_corner_program()?)?;
        assert_eq!(sq.current_answer().clone(), current_rows(&sq, &sym("?")));

        db.run_script("?[x] <- [[1]] :put r {x}", no_params())?;
        let answer_delta = sq.apply_pending_answer()?;
        assert_eq!(
            answer_delta,
            [SignedFact::Minus(Tuple::from_vec(vec![v(1)]))]
                .into_iter()
                .collect()
        );
        assert!(sq.current_answer().is_empty());

        sq.teardown();
        Ok(())
    }

    /// The multi-commit-drain bug (0.9.0 adversarial review, confirmed):
    /// `apply_pending` used to union raw signed facts from EVERY queued
    /// event into one `BTreeSet<SignedFact>` before ever calling
    /// `incremental_eval` — a set that cannot represent "assert then
    /// retract" of the SAME tuple across two different queued events.
    /// `incremental_eval`'s redundancy filter then resolved each raw
    /// fact against the state from BEFORE the whole batch, with no
    /// notion of commit order within it: whichever side happened to
    /// already match pre-batch state survived, the other was silently
    /// dropped. Three repros, each two real commits then ONE
    /// `apply_pending` (never one per commit — the real-commit
    /// differential above always drains one-per-commit, which is
    /// exactly why this hid from it).
    #[test]
    fn apply_pending_nets_multiple_queued_commits_before_evaluating() -> Result<()> {
        let dir = tempfile::tempdir().map_err(|e| miette!("{e}"))?;
        let db = Engine::compose(new_fjall_storage(dir.path())?, Catalog::new())?;

        // Repro 1: put-then-rm of the same absent key nets to no change.
        db.run_script(":create p {x: Int =>}", no_params())?;
        db.run_script(":create r {x: Int =>}", no_params())?;
        let mut sq = db.register_standing("?[x] := *p[x], not *r[x]", no_params())?;
        assert!(sq.current_answer().is_empty());
        db.run_script("?[x] <- [[1]] :put p {x}", no_params())?;
        db.run_script("?[x] <- [[1]] :rm p {x}", no_params())?;
        let delta = sq.apply_pending_answer()?;
        assert!(
            delta.is_empty(),
            "put-then-rm in one drain must net to no change, got {delta:?}"
        );
        assert!(sq.current_answer().is_empty());
        let real: BTreeSet<Tuple> = db
            .run_script("?[x] := *p[x], not *r[x]", no_params())?
            .into_iter()
            .collect();
        assert_eq!(sq.current_answer().clone(), real);
        sq.teardown();

        // Repro 2: p(1) already present; rm-then-put in one drain nets to
        // no change (it stays present).
        let dir2 = tempfile::tempdir().map_err(|e| miette!("{e}"))?;
        let db2 = Engine::compose(new_fjall_storage(dir2.path())?, Catalog::new())?;
        db2.run_script(":create p {x: Int =>}", no_params())?;
        db2.run_script(":create r {x: Int =>}", no_params())?;
        db2.run_script("?[x] <- [[1]] :put p {x}", no_params())?;
        let mut sq2 = db2.register_standing("?[x] := *p[x], not *r[x]", no_params())?;
        assert_eq!(
            sq2.current_answer().clone(),
            [vec![v(1)]].into_iter().map(Tuple::from_vec).collect()
        );
        db2.run_script("?[x] <- [[1]] :rm p {x}", no_params())?;
        db2.run_script("?[x] <- [[1]] :put p {x}", no_params())?;
        let delta2 = sq2.apply_pending_answer()?;
        assert!(
            delta2.is_empty(),
            "rm-then-put in one drain must net to no change, got {delta2:?}"
        );
        assert_eq!(
            sq2.current_answer().clone(),
            [vec![v(1)]].into_iter().map(Tuple::from_vec).collect()
        );
        sq2.teardown();

        // Repro 3, the worst: a KEY-VALUE relation, two puts of the SAME
        // key with DIFFERENT values in one drain must never leave both
        // rows in the maintained state — a structural key-uniqueness
        // violation that poisons every downstream rule.
        let dir3 = tempfile::tempdir().map_err(|e| miette!("{e}"))?;
        let db3 = Engine::compose(new_fjall_storage(dir3.path())?, Catalog::new())?;
        db3.run_script(":create q {k: Int => val: Int}", no_params())?;
        let mut sq3 = db3.register_standing("?[k, val] := *q[k, val]", no_params())?;
        assert!(sq3.current_answer().is_empty());
        db3.run_script("?[k, val] <- [[1, 20]] :put q {k, val}", no_params())?;
        db3.run_script("?[k, val] <- [[1, 30]] :put q {k, val}", no_params())?;
        sq3.apply_pending()?;
        let maintained = sq3.current_answer().clone();
        assert_eq!(
            maintained.len(),
            1,
            "key 1 must appear exactly once, got {maintained:?}"
        );
        let real3: BTreeSet<Tuple> = db3
            .run_script("?[k, val] := *q[k, val]", no_params())?
            .into_iter()
            .collect();
        assert_eq!(maintained, real3);
        assert_eq!(
            maintained,
            [vec![v(1), v(30)]]
                .into_iter()
                .map(Tuple::from_vec)
                .collect()
        );
        sq3.teardown();
        Ok(())
    }

    #[test]
    fn teardown_unregisters_every_subscription() -> Result<()> {
        let dir = tempfile::tempdir().map_err(|e| miette!("{e}"))?;
        let db = Engine::compose(new_fjall_storage(dir.path())?, Catalog::new())?;
        db.run_script(":create p {x: Int =>}", no_params())?;
        db.run_script(":create r {x: Int =>}", no_params())?;
        let sq = StandingQuery::register(&db, hard_corner_program()?)?;
        let ids: Vec<_> = sq.subscriptions.values().map(|s| s.id).collect();
        assert!(!ids.is_empty());
        sq.teardown();
        for id in ids {
            assert!(
                !db.unregister_callback(id)?,
                "id {id:?} should already be gone"
            );
        }
        Ok(())
    }

    /// RAII proof: a `StandingQuery` that goes out of scope WITHOUT calling
    /// `teardown` still unregisters every callback — the `Drop` impl releases
    /// the subscriptions on scope exit, so a forgotten teardown cannot leak a
    /// live registration.
    #[test]
    fn drop_on_scope_exit_unregisters_every_subscription() -> Result<()> {
        let dir = tempfile::tempdir().map_err(|e| miette!("{e}"))?;
        let db = Engine::compose(new_fjall_storage(dir.path())?, Catalog::new())?;
        db.run_script(":create p {x: Int =>}", no_params())?;
        db.run_script(":create r {x: Int =>}", no_params())?;
        let ids: Vec<_> = {
            let sq = StandingQuery::register(&db, hard_corner_program()?)?;
            let ids: Vec<_> = sq.subscriptions.values().map(|s| s.id).collect();
            assert!(!ids.is_empty());
            ids
            // `sq` drops HERE at scope exit — no `teardown()` call.
        };
        for id in ids {
            assert!(
                !db.unregister_callback(id)?,
                "id {id:?} must already be gone after the StandingQuery dropped on scope exit"
            );
        }
        Ok(())
    }

    /// `register_standing`'s translator (`incremental::translate`) does
    /// not itself re-check for recursion — `incremental_eval`'s own
    /// `has_any_cycle` refusal, run as part of `register`'s initial
    /// bootstrap evaluation, is the ONE place that check lives. This
    /// proves the chain actually reaches it end to end through the
    /// public surface on a REAL recursive KyzoScript query (transitive
    /// closure) — a typed `Err`, never a panic or a silently wrong
    /// (e.g. empty) standing query.
    #[test]
    fn register_standing_refuses_a_real_recursive_query() -> Result<()> {
        let dir = tempfile::tempdir().map_err(|e| miette!("{e}"))?;
        let db = Engine::compose(new_fjall_storage(dir.path())?, Catalog::new())?;
        db.run_script(":create edge {a: Int, b: Int =>}", no_params())?;
        let query = "path[a, b] := *edge[a, b]\npath[a, b] := *edge[a, c], path[c, b]\n?[a, b] := path[a, b]";
        let err = match db.register_standing(query, no_params()) {
            Err(e) => e,
            Ok(_) => {
                return Err(miette!(
                    "expected a recursion refusal, got a successful registration"
                ));
            }
        };
        assert!(
            err.to_string().to_lowercase().contains("recursive"),
            "expected a recursion refusal, got: {err}"
        );
        Ok(())
    }

    /// The full public surface, end to end, on a REAL aggregating query:
    /// `Db::register_standing` on a real KyzoScript string (not a
    /// hand-built `StratifiedMagicProgram`), driven by real `:put`/`:rm`
    /// commits, its running answer checked at every step against a fresh
    /// `Db::run_script` of the SAME query text (the real production
    /// evaluator, not a second registration) — and hitting every
    /// aggregation hard case along the way: the current min surviving an
    /// unrelated assertion, the current min itself being retracted (a
    /// rescan, not a signed tally), a group's last member vanishing, and
    /// a brand new group appearing.
    #[test]
    fn register_standing_maintains_a_real_aggregating_query_across_real_commits() -> Result<()> {
        let dir = tempfile::tempdir().map_err(|e| miette!("{e}"))?;
        let db = Engine::compose(new_fjall_storage(dir.path())?, Catalog::new())?;
        db.run_script(":create p {x: Int, y: Int =>}", no_params())?;
        db.run_script("?[x, y] <- [[1, 10], [1, 20]] :put p {x, y}", no_params())?;

        let query = "?[x, min(y)] := *p[x, y]";
        let mut sq = db.register_standing(query, no_params())?;
        let real = || -> Result<BTreeSet<Tuple>> {
            Ok(db.run_script(query, no_params())?.into_iter().collect())
        };

        assert_eq!(
            current_rows(&sq, &sym("?")),
            [vec![v(1), v(10)]]
                .into_iter()
                .map(Tuple::from_vec)
                .collect(),
            "initial snapshot: min(y) for x=1 is 10"
        );
        assert_eq!(current_rows(&sq, &sym("?")), real()?);

        // An unrelated assertion (a new, larger y for the same group)
        // must NOT disturb the current min.
        db.run_script("?[x, y] <- [[1, 30]] :put p {x, y}", no_params())?;
        sq.apply_pending()?;
        assert_eq!(
            current_rows(&sq, &sym("?")),
            [vec![v(1), v(10)]]
                .into_iter()
                .map(Tuple::from_vec)
                .collect(),
            "min(y) unchanged by a larger sibling"
        );
        assert_eq!(current_rows(&sq, &sym("?")), real()?);

        // Retracting the CURRENT min: no per-kind formula covers this,
        // only a re-derivation from the group's remaining members {20, 30}.
        db.run_script("?[x, y] <- [[1, 10]] :rm p {x, y}", no_params())?;
        sq.apply_pending()?;
        assert_eq!(
            current_rows(&sq, &sym("?")),
            [vec![v(1), v(20)]]
                .into_iter()
                .map(Tuple::from_vec)
                .collect(),
            "min(y) rescans to the new minimum, 20"
        );
        assert_eq!(current_rows(&sq, &sym("?")), real()?);

        // A brand new group appears.
        db.run_script("?[x, y] <- [[2, 5]] :put p {x, y}", no_params())?;
        sq.apply_pending()?;
        assert_eq!(
            current_rows(&sq, &sym("?")),
            [vec![v(1), v(20)], vec![v(2), v(5)]]
                .into_iter()
                .map(Tuple::from_vec)
                .collect(),
            "a new group appears with its own min"
        );
        assert_eq!(current_rows(&sq, &sym("?")), real()?);

        // Retracting a group's LAST member: the group vanishes entirely.
        db.run_script("?[x, y] <- [[2, 5]] :rm p {x, y}", no_params())?;
        sq.apply_pending()?;
        assert_eq!(
            current_rows(&sq, &sym("?")),
            [vec![v(1), v(20)]]
                .into_iter()
                .map(Tuple::from_vec)
                .collect(),
            "the emptied group's row vanishes, not just its value"
        );
        assert_eq!(current_rows(&sq, &sym("?")), real()?);

        sq.teardown();
        Ok(())
    }

    // ── The real-commit differential (issue #61's DoD, distinct from the
    // in-memory generative campaigns in laws.rs/incremental.rs): drive a
    // RANDOM SEQUENCE of REAL committed mutations through a REAL Db, and
    // after each one, check the incrementally maintained StandingQuery
    // against a FRESH StandingQuery registered from scratch at that same
    // point — two independent code paths (apply_pending's delta-driven
    // walk vs. register's from-empty bootstrap) over the SAME real
    // storage. Any divergence is a real bug, not an artifact of one
    // algorithm checking itself. ─────────────────────────────────────────

    /// One shape's EDB relations (each with its arity, in `k0..kN`
    /// column-name terms matching `tuple_script`'s convention) and its
    /// REAL KyzoScript query text — registered through
    /// [`Db::register_standing`] and recomputed through
    /// [`Db::run_script`], the SAME public entry points a real user
    /// drives, not a hand-built `StratifiedMagicProgram` fed to a SECOND
    /// internal `StandingQuery::register` call. That distinction is not
    /// pedantry: a translation bug (`translate()` mistranslating the
    /// compiled query) would be invisible to a differential where BOTH
    /// sides run through `translate()` — recomputing via the real query
    /// engine is what actually proves "the maintained answer equals the
    /// query's own answer," not merely "two runs of the SAME translation
    /// agree with each other."
    struct Shape {
        query: &'static str,
        edb: &'static [(&'static str, usize)],
    }

    fn shapes() -> [Shape; 4] {
        [
            // ?(k0) :- p(k0, k1), not r(k0)
            Shape {
                query: "?[k0] := *p[k0, k1], not *r[k0]",
                edb: &[("p", 2), ("r", 1)],
            },
            // mid(k0) :- p(k0, k1), not r(k0); ?(k0) :- mid(k0), not s(k0)
            Shape {
                query: "mid[k0] := *p[k0, k1], not *r[k0]\n?[k0] := mid[k0], not *s[k0]",
                edb: &[("p", 2), ("r", 1), ("s", 1)],
            },
            // ?(k0, k1) :- p(k0, k1), r2(k0, k1)
            Shape {
                query: "?[k0, k1] := *p[k0, k1], *r2[k0, k1]",
                edb: &[("p", 2), ("r2", 2)],
            },
            // ?(k0, min(k1)) :- p(k0, k1) — aggregation, `min` deliberately
            // (the hardest kind: retracting the current min has no
            // per-kind incremental formula, only a group re-derivation).
            Shape {
                query: "?[k0, min(k1)] := *p[k0, k1]",
                edb: &[("p", 2)],
            },
        ]
    }

    fn create_relation(
        db: &Engine<crate::store::fjall::FjallStorage>,
        name: &str,
        arity: usize,
    ) -> Result<()> {
        let cols = (0..arity)
            .map(|i| format!("k{i}: Int"))
            .collect::<Vec<_>>()
            .join(", ");
        db.run_script(&format!(":create {name} {{{cols} =>}}"), no_params())?;
        Ok(())
    }

    fn tuple_script(op: &str, rel: &str, arity: usize, row: &[i64]) -> String {
        let vars: Vec<String> = (0..arity).map(|i| format!("k{i}")).collect();
        let vals: Vec<String> = row.iter().map(|v| v.to_string()).collect();
        format!(
            "?[{}] <- [[{}]] {op} {rel} {{{}}}",
            vars.join(", "),
            vals.join(", "),
            vars.join(", "),
        )
    }

    #[test]
    fn incremental_matches_recompute_across_real_commit_sequences() -> Result<()> {
        let mut rng: u64 = 0xC0FF_EE00_DEAD_BEEF;
        let mut next_u64 = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        let mut next_range = |n: u64| next_u64() % n;

        let mut cases = 0;
        for shape in shapes() {
            for _iteration in 0..8 {
                let dir = tempfile::tempdir().map_err(|e| miette!("{e}"))?;
                let db = Engine::compose(new_fjall_storage(dir.path())?, Catalog::new())?;
                // `live: what every EDB relation ACTUALLY holds right now,
                // mirrored in-process so a `Minus` picks a real victim.
                let mut live: BTreeMap<&str, BTreeSet<Vec<i64>>> = BTreeMap::new();
                for &(rel, arity) in shape.edb {
                    create_relation(&db, rel, arity)?;
                    let mut rows = BTreeSet::new();
                    let n = next_range(4);
                    for _ in 0..n {
                        let mut row = Vec::with_capacity(arity);
                        for _ in 0..arity {
                            row.push(crate::rules::convert::i64_from_u64_nonneg_fitting(next_range(3)));
                        }
                        if rows.insert(row.clone()) {
                            db.run_script(&tuple_script(":put", rel, arity, &row), no_params())?;
                        }
                    }
                    live.insert(rel, rows);
                }

                let mut incremental = db.register_standing(shape.query, no_params())?;

                for _commit in 0..5 {
                    let edb_len = match u64::try_from(shape.edb.len()) {
                        Ok(v) => v,
                        Err(_) => {
                            // Published floor — convert/refuse door preferred when total.
                            0
                        },
                    };
                    let edb_idx = crate::rules::convert::usize_from_u64_fitting(next_range(edb_len.max(1)));
                    let (rel, arity) = shape.edb[edb_idx % shape.edb.len()];
                    let existing: Vec<Vec<i64>> = live[rel].iter().cloned().collect();
                    if !existing.is_empty() && next_range(2) == 0 {
                        let exist_len = match u64::try_from(existing.len()) {
                            Ok(v) => v,
                            Err(_) => {
                                // Published floor — convert/refuse door preferred when total.
                                0
                            },
                        };
                        let victim_idx = crate::rules::convert::usize_from_u64_fitting(next_range(exist_len.max(1)));
                        let victim = existing[victim_idx % existing.len()].clone();
                        db.run_script(&tuple_script(":rm", rel, arity, &victim), no_params())?;
                        live.get_mut(rel)
                            .ok_or_else(|| miette!("get_mut"))?
                            .remove(&victim);
                    } else {
                        let mut row = Vec::with_capacity(arity);
                        for _ in 0..arity {
                            row.push(crate::rules::convert::i64_from_u64_nonneg_fitting(next_range(3)));
                        }
                        db.run_script(&tuple_script(":put", rel, arity, &row), no_params())?;
                        live.get_mut(rel)
                            .ok_or_else(|| miette!("get_mut"))?
                            .insert(row);
                    }

                    incremental.apply_pending()?;
                    // The REAL recompute: the SAME query text through the
                    // real production evaluator (parse -> normalize ->
                    // stratify -> magic -> compile -> RA eval), never a
                    // second `register_standing`/`translate()` call — see
                    // the module-level note on `Shape` for why that
                    // distinction is load-bearing.
                    let recomputed: BTreeSet<Tuple> = db
                        .run_script(shape.query, no_params())?
                        .into_iter()
                        .collect();
                    assert_eq!(
                        current_rows(&incremental, &sym("?")),
                        recomputed,
                        "shape '{}', commit {_commit}: mismatch on the entry relation",
                        shape.query,
                    );
                    cases += 1;
                }
                incremental.teardown();
            }
        }
        assert!(
            cases > 100,
            "expected a real generative campaign, ran {cases}"
        );
        Ok(())
    }

    /// The test gap the 0.9.0 adversarial review named directly: every
    /// OTHER real-commit differential in this module calls
    /// `apply_pending` once PER commit, so events never accumulate —
    /// exactly the one path `apply_pending`'s pull-based design exists
    /// to support (poll less often than every commit) was never
    /// exercised. This campaign issues a random BATCH of 1-4 real
    /// commits — puts of new keys, puts that CHANGE an existing key's
    /// value (the repro-3 worst case), and retractions, all mixed
    /// together and free to touch the SAME key more than once in one
    /// batch — before a single `apply_pending`, then checks the
    /// maintained answer against a fresh `db.run_script` recompute. Runs
    /// against a genuine key-VALUE relation (none of `shapes()`'s EDB
    /// relations have value columns at all, so none of them could have
    /// caught the worst repro) with both a plain projection and an
    /// aggregation over it.
    #[test]
    fn apply_pending_matches_recompute_across_batched_multi_commit_drains() -> Result<()> {
        let mut rng: u64 = 0xBADD_ECAF_5EED_1234;
        let mut next_u64 = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        let mut next_range = |n: u64| next_u64() % n;

        let queries = ["?[k, val] := *q[k, val]", "?[k, sum(val)] := *q[k, val]"];

        let mut cases = 0;
        for query in queries {
            for _iteration in 0..20 {
                let dir = tempfile::tempdir().map_err(|e| miette!("{e}"))?;
                let db = Engine::compose(new_fjall_storage(dir.path())?, Catalog::new())?;
                db.run_script(":create q {k: Int => val: Int}", no_params())?;

                // `live`: the CURRENT key -> value mapping, mirrored
                // in-process (a key-value relation has at most one row
                // per key, unlike the key-only `live` sets elsewhere in
                // this module).
                let mut live: BTreeMap<u64, u64> = BTreeMap::new();
                for _ in 0..next_range(4) {
                    let k = next_range(3);
                    let v = next_range(5);
                    db.run_script(
                        &format!("?[k, val] <- [[{k}, {v}]] :put q {{k, val}}"),
                        no_params(),
                    )?;
                    live.insert(k, v);
                }

                let mut sq = db.register_standing(query, no_params())?;

                for _batch in 0..8 {
                    let batch_size = 1 + next_range(4);
                    for _op in 0..batch_size {
                        let k = next_range(3);
                        if live.contains_key(&k) && next_range(3) == 0 {
                            db.run_script(&format!("?[k] <- [[{k}]] :rm q {{k}}"), no_params())?;
                            live.remove(&k);
                        } else {
                            let v = next_range(5);
                            db.run_script(
                                &format!("?[k, val] <- [[{k}, {v}]] :put q {{k, val}}"),
                                no_params(),
                            )?;
                            live.insert(k, v);
                        }
                    }

                    sq.apply_pending()?;
                    let recomputed: BTreeSet<Tuple> =
                        db.run_script(query, no_params())?.into_iter().collect();
                    let maintained = sq.current_answer().clone();
                    assert_eq!(
                        maintained, recomputed,
                        "query '{query}', batch {_batch}: mismatch after a {batch_size}-commit \
                         drain (live keys: {live:?})",
                    );
                    // The structural property repro 3 broke, checked
                    // directly rather than only through equality with
                    // `recomputed` (which could coincidentally still
                    // hold if the real answer also happened to have a
                    // duplicate key — it never will, but this is the
                    // property actually being defended): the plain
                    // projection can never carry two rows for the same
                    // key.
                    if query == queries[0] {
                        let keys: BTreeSet<&DataValue> =
                            maintained.iter().map(|row| &row[0]).collect();
                        assert_eq!(
                            keys.len(),
                            maintained.len(),
                            "duplicate key in maintained state: {maintained:?}"
                        );
                    }
                    cases += 1;
                }
                sq.teardown();
            }
        }
        assert!(
            cases > 100,
            "expected a rich batched-drain campaign, ran {cases}"
        );
        Ok(())
    }
}
