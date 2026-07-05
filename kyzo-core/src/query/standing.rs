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
                        // A row present in BOTH sets is a redundant re-put
                        // of identical content at the same key (no value
                        // columns changed, or a key-only relation like the
                        // test fixtures below) — net effect is no change,
                        // not a Minus-then-filtered-Plus pair. Computing
                        // the difference here, not raw per-set facts, is
                        // what keeps that case from ever reaching
                        // `incremental_eval` as a spurious `Minus`.
                        let new_set: BTreeSet<Tuple> = new.rows.into_iter().collect();
                        let old_set: BTreeSet<Tuple> = old.rows.into_iter().collect();
                        for row in new_set.difference(&old_set) {
                            entry.insert(SignedFact::Plus(row.clone()));
                        }
                        for row in old_set.difference(&new_set) {
                            entry.insert(SignedFact::Minus(row.clone()));
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

    fn rel_atom(name: &str, args: Vec<&str>, negated: bool) -> MagicAtom {
        let atom = MagicRelationApplyAtom {
            name: sym(name),
            args: args.into_iter().map(sym).collect(),
            validity: None,
            span: SourceSpan::default(),
        };
        if negated {
            MagicAtom::NegatedRelation(atom)
        } else {
            MagicAtom::Relation(atom)
        }
    }
    fn magic_inline(head: Vec<&str>, body: Vec<MagicAtom>) -> MagicInlineRule {
        let aggr = vec![None; head.len()];
        MagicInlineRule {
            head: head.into_iter().map(sym).collect(),
            aggr,
            body,
        }
    }
    fn one_stratum_program(defs: Vec<(&str, Vec<MagicInlineRule>)>) -> StratifiedMagicProgram {
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
            .expect("well-formed test program")
    }

    /// `?(x) :- p(x), not r(x)` — the hard-corner scenario, from a REAL
    /// Db/callback registration through to a real commit's delta.
    fn hard_corner_program() -> StratifiedMagicProgram {
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
    fn register_snapshots_current_state_then_apply_pending_tracks_real_commits() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::new(new_fjall_storage(dir.path()).unwrap()).unwrap();
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
        let dir = tempfile::tempdir().unwrap();
        let db = Db::new(new_fjall_storage(dir.path()).unwrap()).unwrap();
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

    // ── The real-commit differential (issue #61's DoD, distinct from the
    // in-memory generative campaigns in laws.rs/incremental.rs): drive a
    // RANDOM SEQUENCE of REAL committed mutations through a REAL Db, and
    // after each one, check the incrementally maintained StandingQuery
    // against a FRESH StandingQuery registered from scratch at that same
    // point — two independent code paths (apply_pending's delta-driven
    // walk vs. register's from-empty bootstrap) over the SAME real
    // storage. Any divergence is a real bug, not an artifact of one
    // algorithm checking itself. ─────────────────────────────────────────

    fn rule_atom(name: &str, args: Vec<&str>, negated: bool) -> MagicAtom {
        let atom = crate::data::program::MagicRuleApplyAtom {
            name: MagicSymbol::Muggle { inner: sym(name) },
            args: args.into_iter().map(sym).collect(),
            span: SourceSpan::default(),
        };
        if negated {
            MagicAtom::NegatedRule(atom)
        } else {
            MagicAtom::Rule(atom)
        }
    }

    /// One shape's EDB relations, each with its arity (in var-count terms
    /// — every EDB relation here is all-key, no value columns, matching
    /// `hard_corner_program`'s own convention).
    struct Shape {
        program: fn() -> StratifiedMagicProgram,
        edb: &'static [(&'static str, usize)],
    }

    fn shapes() -> [Shape; 3] {
        // ?(x) :- p(x, y), not r(x)
        fn shape_a() -> StratifiedMagicProgram {
            one_stratum_program(vec![(
                "?",
                vec![magic_inline(
                    vec!["X"],
                    vec![
                        rel_atom("p", vec!["X", "Y"], false),
                        rel_atom("r", vec!["X"], true),
                    ],
                )],
            )])
        }
        // mid(x) :- p(x, y), not r(x)
        // ?(x) :- mid(x), not s(x)
        fn shape_b() -> StratifiedMagicProgram {
            one_stratum_program(vec![
                (
                    "mid",
                    vec![magic_inline(
                        vec!["X"],
                        vec![
                            rel_atom("p", vec!["X", "Y"], false),
                            rel_atom("r", vec!["X"], true),
                        ],
                    )],
                ),
                (
                    "?",
                    vec![magic_inline(
                        vec!["X"],
                        vec![
                            rule_atom("mid", vec!["X"], false),
                            rel_atom("s", vec!["X"], true),
                        ],
                    )],
                ),
            ])
        }
        // ?(x, y) :- p(x, y), r2(x, y)
        fn shape_c() -> StratifiedMagicProgram {
            one_stratum_program(vec![(
                "?",
                vec![magic_inline(
                    vec!["X", "Y"],
                    vec![
                        rel_atom("p", vec!["X", "Y"], false),
                        rel_atom("r2", vec!["X", "Y"], false),
                    ],
                )],
            )])
        }
        [
            Shape {
                program: shape_a,
                edb: &[("p", 2), ("r", 1)],
            },
            Shape {
                program: shape_b,
                edb: &[("p", 2), ("r", 1), ("s", 1)],
            },
            Shape {
                program: shape_c,
                edb: &[("p", 2), ("r2", 2)],
            },
        ]
    }

    fn create_relation(db: &Db<crate::storage::fjall::FjallStorage>, name: &str, arity: usize) {
        let cols = (0..arity)
            .map(|i| format!("k{i}: Int"))
            .collect::<Vec<_>>()
            .join(", ");
        db.run_script(&format!(":create {name} {{{cols} =>}}"), no_params())
            .unwrap();
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
    fn incremental_matches_recompute_across_real_commit_sequences() {
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
                let dir = tempfile::tempdir().unwrap();
                let db = Db::new(new_fjall_storage(dir.path()).unwrap()).unwrap();
                // `live: what every EDB relation ACTUALLY holds right now,
                // mirrored in-process so a `Minus` picks a real victim.
                let mut live: BTreeMap<&str, BTreeSet<Vec<i64>>> = BTreeMap::new();
                for &(rel, arity) in shape.edb {
                    create_relation(&db, rel, arity);
                    let mut rows = BTreeSet::new();
                    let n = next_range(4);
                    for _ in 0..n {
                        let row: Vec<i64> = (0..arity).map(|_| next_range(3) as i64).collect();
                        if rows.insert(row.clone()) {
                            db.run_script(&tuple_script(":put", rel, arity, &row), no_params())
                                .unwrap();
                        }
                    }
                    live.insert(rel, rows);
                }

                let mut incremental = StandingQuery::register(&db, (shape.program)()).unwrap();

                for _commit in 0..5 {
                    let (rel, arity) = shape.edb[next_range(shape.edb.len() as u64) as usize];
                    let existing: Vec<Vec<i64>> = live[rel].iter().cloned().collect();
                    if !existing.is_empty() && next_range(2) == 0 {
                        let victim = existing[next_range(existing.len() as u64) as usize].clone();
                        db.run_script(&tuple_script(":rm", rel, arity, &victim), no_params())
                            .unwrap();
                        live.get_mut(rel).unwrap().remove(&victim);
                    } else {
                        let row: Vec<i64> = (0..arity).map(|_| next_range(3) as i64).collect();
                        db.run_script(&tuple_script(":put", rel, arity, &row), no_params())
                            .unwrap();
                        live.get_mut(rel).unwrap().insert(row);
                    }

                    incremental.apply_pending().unwrap();
                    let fresh = StandingQuery::register(&db, (shape.program)()).unwrap();

                    let rel_names: BTreeSet<Symbol> = incremental
                        .state
                        .keys()
                        .chain(fresh.state.keys())
                        .cloned()
                        .collect();
                    for r in rel_names {
                        assert_eq!(
                            incremental.current(&r).cloned().unwrap_or_default(),
                            fresh.current(&r).cloned().unwrap_or_default(),
                            "shape {:?}, commit {_commit}: mismatch on relation '{r}'",
                            shape.edb,
                        );
                    }
                    fresh.teardown();
                    cases += 1;
                }
                incremental.teardown();
            }
        }
        assert!(
            cases > 100,
            "expected a real generative campaign, ran {cases}"
        );
    }
}
