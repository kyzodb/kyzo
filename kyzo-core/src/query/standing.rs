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
//! [`crate::query::incremental`]'s translator and evaluator, and on
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
//! ## `CallbackEvent` → `SignedFact`, the one real asymmetry
//!
//! `CallbackOp::Put`'s `new`/`old` `NamedRows` are two INDEPENDENT row
//! sets sharing the relation's full key+value header (never index-paired
//! — `runtime/mutate.rs`'s `collect_mutations` builds them as separate
//! lists, not a zipped diff): every "new" row is a `Plus`, every "old"
//! row is a `Minus`. `CallbackOp::Rm` is NOT symmetric: its "new" side is
//! built from the relation's KEY-ONLY columns (`k_bindings`, not
//! `kv_bindings`) — a bare key, not a real row this program's arity would
//! match — so an `Rm` event contributes `Minus` from "old" only; "new" is
//! never a fact.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc::Receiver;

use miette::{Result, miette};

use crate::data::symb::Symbol;
use crate::data::tuple::Tuple;
use crate::query::incremental::{self, IncrementalProgram, MaintainedState};
use crate::query::ra::temporal::SignedFact;
use crate::runtime::callback::{CallbackEvent, CallbackOp};
use crate::runtime::db::Db;
use crate::runtime::relation::get_relation;
use crate::storage::Storage;

/// One EDB dependency's live subscription: the callback id (for
/// [`StandingQuery::teardown`]) and the receiver it delivers on.
struct Subscription {
    id: u32,
    receiver: Receiver<CallbackEvent>,
}

/// A live standing query: a translated program, its persistently
/// maintained state, and one subscription per EDB relation it depends
/// on. Construction (via [`StandingQuery::register`]) is the only way to
/// get one — there is no bare-fields constructor, so a `StandingQuery`
/// that exists is always both subscribed and snapshot-initialized.
pub(crate) struct StandingQuery<S: Storage> {
    db: Db<S>,
    program: IncrementalProgram,
    state: MaintainedState,
    subscriptions: BTreeMap<Symbol, Subscription>,
}

impl<S: Storage> StandingQuery<S> {
    /// Register a standing query: translate the compiled program, then
    /// subscribe to and snapshot every EDB relation it depends on, in
    /// that order (the snapshot-consistency argument in the module doc
    /// depends on this order — subscribe first, read second).
    pub(crate) fn register(
        db: &Db<S>,
        magic: crate::data::program::StratifiedMagicProgram,
    ) -> Result<Self> {
        let program = incremental::translate(magic).map_err(|e| miette!("{e}"))?;
        let edb = incremental::edb_relations_pub(&program);

        let mut subscriptions = BTreeMap::new();
        for rel in &edb {
            let (id, receiver) = db.register_callback(rel.name.as_str());
            subscriptions.insert(rel.clone(), Subscription { id, receiver });
        }

        let tx = db.storage.read_tx()?;
        let mut state: MaintainedState = BTreeMap::new();
        for rel in &edb {
            let handle = get_relation(&tx, rel.name.as_str())?;
            let rows: BTreeSet<Tuple> = handle.scan_all(&tx).collect::<Result<_>>()?;
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
            incremental::incremental_eval(&program, &edb_only_state, &seed_patch)
                .map_err(|e| miette!("{e}"))?;

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
    pub(crate) fn current(&self, rel: &Symbol) -> Option<&BTreeSet<Tuple>> {
        self.state.get(rel)
    }

    /// Drain every subscribed relation's pending callback events, fold
    /// them into one signed EDB patch, and apply it — returning the
    /// signed delta every relation (EDB and IDB alike) underwent. An
    /// empty return means nothing was pending.
    pub(crate) fn apply_pending(&mut self) -> Result<BTreeMap<Symbol, BTreeSet<SignedFact>>> {
        let mut edb_patch: BTreeMap<Symbol, BTreeSet<SignedFact>> = BTreeMap::new();
        for (rel, sub) in &self.subscriptions {
            let entry = edb_patch.entry(rel.clone()).or_default();
            while let Ok((op, new, old)) = sub.receiver.try_recv() {
                match op {
                    CallbackOp::Put => {
                        for row in new.rows {
                            entry.insert(SignedFact::Plus(row));
                        }
                        for row in old.rows {
                            entry.insert(SignedFact::Minus(row));
                        }
                    }
                    CallbackOp::Rm => {
                        // `new` here is bare keys (k_bindings), never a
                        // real row this program's arity matches — only
                        // `old` (the full removed row) is a fact.
                        for row in old.rows {
                            entry.insert(SignedFact::Minus(row));
                        }
                    }
                }
            }
        }
        if edb_patch.values().all(BTreeSet::is_empty) {
            return Ok(BTreeMap::new());
        }
        let (deltas, new_state) =
            incremental::incremental_eval(&self.program, &self.state, &edb_patch)
                .map_err(|e| miette!("{e}"))?;
        self.state = new_state;
        Ok(deltas)
    }

    /// Unregister every underlying per-relation callback. A `StandingQuery`
    /// dropped without calling this leaks nothing worse than an idle
    /// channel the registry prunes on its own next disconnect check
    /// (`runtime/callback.rs`'s "lossy by disconnect" contract) — this
    /// method just makes the teardown immediate and explicit instead of
    /// waiting on that.
    pub(crate) fn teardown(self) {
        for sub in self.subscriptions.into_values() {
            self.db.unregister_callback(sub.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::program::{
        MagicAtom, MagicInlineRule, MagicProgram, MagicRelationApplyAtom, MagicRulesOrFixed,
        MagicSymbol, StratifiedMagicProgram,
    };
    use crate::data::span::SourceSpan;
    use crate::data::value::{DataValue, Num};
    use crate::storage::fjall::new_fjall_storage;

    fn sym(name: &str) -> Symbol {
        Symbol::new(name, SourceSpan::default())
    }
    fn v(i: i64) -> DataValue {
        DataValue::Num(Num::Int(i))
    }
    fn no_params() -> BTreeMap<String, DataValue> {
        BTreeMap::new()
    }
    fn tempdir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kyzo-standing-query-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    /// `?(x) :- p(x), not r(x)` — the hard-corner scenario, from a REAL
    /// Db/callback registration through to a real commit's delta.
    fn hard_corner_program() -> StratifiedMagicProgram {
        let rel = |name: &str| {
            MagicAtom::Relation(MagicRelationApplyAtom {
                name: sym(name),
                args: vec![sym("X")],
                validity: None,
                span: SourceSpan::default(),
            })
        };
        let neg_rel = |name: &str| {
            MagicAtom::NegatedRelation(MagicRelationApplyAtom {
                name: sym(name),
                args: vec![sym("X")],
                validity: None,
                span: SourceSpan::default(),
            })
        };
        let rule = MagicInlineRule {
            head: vec![sym("X")],
            aggr: vec![None],
            body: vec![rel("p"), neg_rel("r")],
        };
        let prog = BTreeMap::from([(
            MagicSymbol::Muggle { inner: sym("?") },
            MagicRulesOrFixed::Rules { rules: vec![rule] },
        )]);
        StratifiedMagicProgram::from_execution_order(vec![MagicProgram { prog }])
            .expect("well-formed test program")
    }

    #[test]
    fn register_snapshots_current_state_then_apply_pending_tracks_real_commits() {
        let db = Db::new(new_fjall_storage(tempdir()).unwrap()).unwrap();
        db.run_script(":create p {x: Int =>}", no_params()).unwrap();
        db.run_script(":create r {x: Int =>}", no_params()).unwrap();
        // p(1) exists, r is empty: q(1) should already hold at registration.
        db.run_script("?[x] <- [[1]] :put p {x}", no_params())
            .unwrap();

        let mut sq = StandingQuery::register(&db, hard_corner_program()).unwrap();
        assert_eq!(
            sq.current(&sym("?")).cloned().unwrap_or_default(),
            [vec![v(1)]].into_iter().collect(),
            "q(1) must already hold from the pre-registration snapshot"
        );

        // Nothing committed since registration: apply_pending is a no-op.
        assert!(sq.apply_pending().unwrap().is_empty());

        // The hard corner: retract r's ABSENCE by asserting into it —
        // q(1) must be retracted.
        db.run_script("?[x] <- [[1]] :put r {x}", no_params())
            .unwrap();
        let deltas = sq.apply_pending().unwrap();
        assert_eq!(
            deltas.get(&sym("?")).cloned().unwrap_or_default(),
            [SignedFact::Minus(vec![v(1)])].into_iter().collect()
        );
        assert!(sq.current(&sym("?")).unwrap().is_empty());

        // The mirror: retracting r(1) makes q(1) hold again.
        db.run_script("?[x] <- [[1]] :rm r {x}", no_params())
            .unwrap();
        let deltas = sq.apply_pending().unwrap();
        assert_eq!(
            deltas.get(&sym("?")).cloned().unwrap_or_default(),
            [SignedFact::Plus(vec![v(1)])].into_iter().collect()
        );
        assert_eq!(
            sq.current(&sym("?")).cloned().unwrap_or_default(),
            [vec![v(1)]].into_iter().collect()
        );

        sq.teardown();
    }

    #[test]
    fn teardown_unregisters_every_subscription() {
        let db = Db::new(new_fjall_storage(tempdir()).unwrap()).unwrap();
        db.run_script(":create p {x: Int =>}", no_params()).unwrap();
        db.run_script(":create r {x: Int =>}", no_params()).unwrap();
        let sq = StandingQuery::register(&db, hard_corner_program()).unwrap();
        let ids: Vec<u32> = sq.subscriptions.values().map(|s| s.id).collect();
        assert!(!ids.is_empty());
        sq.teardown();
        for id in ids {
            assert!(
                !db.unregister_callback(id),
                "id {id} should already be gone"
            );
        }
    }
}
