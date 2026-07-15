/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Integrity constraints as denial rules.
//!
//! A constraint is a **named pure query that must derive nothing**: the
//! Datalog `⊥ :- body` shape. FK, CHECK, and secondary uniqueness are all
//! the same species — `::constraint create <name> { <body> }` declares a
//! body whose satisfying rows are *violations*, and a transaction commits
//! iff every constraint's body is empty against the post-write state.
//!
//! Mechanics, stated plainly:
//!
//! - **Catalog storage.** The body is raw KyzoScript source, mirrored into
//!   the catalog row ([`ConstraintRef`]) of *every* stored relation it
//!   reads, so an FK fires both when a child appears and when its parent
//!   disappears. Parsed once per session (the trigger convention; parsed
//!   substances in the catalog are the Phase C end state).
//! - **Enforcement point.** The mutation pipeline notes the constraints of
//!   every relation it touches ([`SessionTx::note_constraints`]); after the
//!   top-level query and its whole trigger cascade have run — and before
//!   commit — [`Db::enforce_constraints`] evaluates each noted constraint
//!   once, read-only, against the transaction's own write set
//!   (`WriteTx: ReadTx`, so post-write state is visible). A non-empty
//!   result is a typed, spanned [`ConstraintViolation`] naming the
//!   constraint and its witness rows; the abort rolls the whole
//!   transaction back.
//! - **Budget-armed.** Constraint bodies evaluate under the session's
//!   [`ScriptOptions`] budget; a body that exceeds it is a typed refusal
//!   (wrapped to name the constraint), never a hang.
//! - **Deterministic witnesses.** Constraints are checked in name order and
//!   witness rows are sorted and deduped before reporting, so the same
//!   violation yields the same refusal at any thread count. At most
//!   [`WITNESS_CAP`] witnesses are shown; the total count is always
//!   reported.
//! - **Creation over existing data.** `::constraint create` evaluates the
//!   body over the full current state inside the creating transaction and
//!   refuses creation with witnesses if anything matches — a constraint
//!   that current data violates never comes into being.
//!
//! # Named limitation (v1): constraints are checked at `cur_vld` only
//!
//! A constraint body is evaluated **once, against post-write state at the
//! session's `cur_vld`**. On a plain (non-time-travel) relation this is total:
//! every row that exists is visible. On a **validity (time-travel) relation**
//! it is not: a `:put` asserting a fact whose validity begins in the *future*
//! is invisible to the body's default-now scan, so a violation that only
//! materializes once wall-clock passes that timestamp is **not** caught at
//! commit — there is no later transaction to re-check it. This is the flip
//! side of the design's explicit non-goal: v1 constraints do **not** reach
//! across history on their own — `deny … @T` over a validity scan now
//! compiles and evaluates (story #86 closed the engine's negation-over-
//! time-travel gap), so a body MAY name a fixed historical coordinate, but
//! the constraint is still checked only once, at creation and at THIS
//! session's `cur_vld`, never re-run at other instants automatically. A
//! constraint that must hold across time must be written with explicit `@`
//! validity qualifiers in its body; the natural now-scoped FK/CHECK shape
//! guards the present instant only. This boundary is stated, not silently
//! assumed.

use std::collections::{BTreeMap, BTreeSet};

use miette::{Diagnostic, Result, WrapErr, bail};
use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

use crate::data::program::{
    FixedRuleArg, InputAtom, InputInlineRulesOrFixed, InputProgram, QueryOutOptions,
};
use crate::data::span::SourceSpan;
use crate::data::symb::Symbol;
use crate::data::value::Tuple;
use crate::data::value::{DataValue, ValidityTs};
use crate::fixed_rule::NamedRows;
use crate::parse::parse_script;
use crate::runtime::db::{Db, ScriptOptions, SessionTx};
use crate::runtime::relation::{
    AccessLevel, ConstraintRef, InsufficientAccessLevel, get_relation, list_relations,
    write_relation_row,
};
use crate::storage::temp::TempTx;
use crate::storage::{ReadTx, Storage, WriteTx};

/// How many witness rows a refusal names. The rows shown are the smallest
/// in value order (witnesses are sorted before capping), so the selection
/// is deterministic; the refusal always carries the *total* count.
pub(crate) const WITNESS_CAP: usize = 8;

/// How many commit attempts a constraint catalog op replays on a typed
/// conflict (the same policy as [`Db`]'s script path).
const MAX_COMMIT_ATTEMPTS: usize = 32;

/// A transaction was denied: an integrity constraint's body is satisfiable
/// against the post-write state. Carries the violating rows (the body's
/// satisfying bindings), sorted, capped at [`WITNESS_CAP`].
#[derive(Debug, Error, Diagnostic)]
#[error(
    "integrity constraint '{name}' denies this transaction: \
     {total} violating row(s); witnesses: {witnesses:?}"
)]
#[diagnostic(code(tx::constraint_violation))]
#[diagnostic(help("the transaction was aborted whole; no writes were kept"))]
pub(crate) struct ConstraintViolation {
    pub(crate) name: String,
    /// The number of *distinct* violating rows (witnesses are deduped; the
    /// shown `witnesses` list is further capped at [`WITNESS_CAP`]).
    pub(crate) total: usize,
    pub(crate) witnesses: Vec<Tuple>,
    #[source_code]
    pub(crate) body: String,
    #[label("this denial rule matched")]
    pub(crate) span: SourceSpan,
}

/// `::constraint create` over data that already violates the body. The
/// constraint does not come into being; the witnesses name the offenders.
#[derive(Debug, Error, Diagnostic)]
#[error(
    "cannot create integrity constraint '{name}': existing data violates it \
     ({total} row(s)); witnesses: {witnesses:?}"
)]
#[diagnostic(code(tx::constraint_rejected_on_creation))]
#[diagnostic(help("repair the offending rows first, then create the constraint"))]
pub(crate) struct ConstraintRejectedOnCreation {
    pub(crate) name: String,
    pub(crate) total: usize,
    pub(crate) witnesses: Vec<Tuple>,
    #[source_code]
    pub(crate) body: String,
    #[label("this denial rule already matches")]
    pub(crate) span: SourceSpan,
}

/// A constraint body that is not a pure query. Checked at creation and
/// re-checked defensively at enforcement (catalog bytes are claims, not
/// proofs).
#[derive(Debug, Error, Diagnostic)]
#[error("integrity constraint '{0}' must be a pure query: {1}")]
#[diagnostic(code(tx::constraint_not_pure))]
pub(crate) struct ConstraintNotPure(pub(crate) String, pub(crate) &'static str);

/// A constraint body reading no stored relation: it would never be checked
/// (enforcement is keyed off touched relations), so it is refused rather
/// than admitted as dead law.
#[derive(Debug, Error, Diagnostic)]
#[error("integrity constraint '{0}' reads no stored relation, so it could never be checked")]
#[diagnostic(code(tx::constraint_reads_nothing))]
pub(crate) struct ConstraintReadsNothing(pub(crate) String);

/// A constraint body reading a session-local temp relation. Constraints
/// are persistent; a body depending on `_`-prefixed data is unenforceable
/// outside the declaring session.
#[derive(Debug, Error, Diagnostic)]
#[error(
    "integrity constraint '{0}' reads temp relation '{1}'; constraints are \
     persistent and cannot depend on session-local relations"
)]
#[diagnostic(code(tx::constraint_on_temp_relation))]
pub(crate) struct ConstraintOnTempRelation(pub(crate) String, pub(crate) String);

/// Constraint names are one global namespace (a constraint spans every
/// relation its body reads).
#[derive(Debug, Error, Diagnostic)]
#[error("an integrity constraint named '{0}' already exists (attached to relation '{1}')")]
#[diagnostic(code(tx::constraint_name_taken))]
pub(crate) struct ConstraintNameTaken(pub(crate) String, pub(crate) String);

/// `::constraint drop` of a name no relation carries.
#[derive(Debug, Error, Diagnostic)]
#[error("no integrity constraint named '{0}' exists")]
#[diagnostic(code(tx::no_such_constraint))]
pub(crate) struct NoSuchConstraint(pub(crate) String);

/// Refuse a constraint body that is not a pure query. `:put`/`:rm`/… would
/// make the check a mutation; `:assert` would race the denial semantics;
/// `:limit 0` would silently hide every violation — all refused, loudly.
fn validate_constraint_purity(name: &str, opts: &QueryOutOptions) -> Result<()> {
    if opts.store_relation.is_some() {
        bail!(ConstraintNotPure(
            name.to_string(),
            "its body mutates a stored relation (:put/:rm/:create/…)"
        ));
    }
    if opts.assertion.is_some() {
        bail!(ConstraintNotPure(
            name.to_string(),
            "its body uses :assert (the denial semantics already assert emptiness)"
        ));
    }
    if opts.limit.is_some() || opts.offset.is_some() {
        bail!(ConstraintNotPure(
            name.to_string(),
            "its body uses :limit/:offset, which would truncate violation detection"
        ));
    }
    if opts.timeout.is_some() || opts.sleep.is_some() {
        // A constraint runs under the session's budget; a per-body :timeout is
        // meaningless AND a panic vector — the parser bounds :timeout only by
        // `> 0`, so a huge value overflows `Duration::from_secs_f64` in
        // `build_budget`. Refuse it here (and defensively at enforcement) so a
        // constraint body can never carry the general-query-path panic into
        // the denial check. `:sleep` is refused for the same purity reason.
        bail!(ConstraintNotPure(
            name.to_string(),
            "its body uses :timeout/:sleep (a denial rule runs under the session \
             budget; a per-body deadline is meaningless and an unbounded value \
             would overflow the duration conversion)"
        ));
    }
    Ok(())
}

/// Every stored relation a program's rule bodies read: positional
/// (`*rel[…]`) and named-field (`*rel{…}`) atoms — through negation,
/// conjunction, and disjunction — index-search atoms, and fixed-rule
/// stored-relation arguments. Rule atoms reference in-program rules, not
/// stored relations, and contribute nothing.
fn stored_read_set(prog: &InputProgram) -> BTreeSet<SmartString<LazyCompact>> {
    fn collect_atom(atom: &InputAtom, out: &mut BTreeSet<SmartString<LazyCompact>>) {
        match atom {
            InputAtom::Relation { inner } => {
                out.insert(inner.name.name.clone());
            }
            InputAtom::NamedFieldRelation { inner } => {
                out.insert(inner.name.name.clone());
            }
            InputAtom::Search { inner } => {
                out.insert(inner.relation.name.clone());
            }
            InputAtom::Negation { inner, .. } => collect_atom(inner, out),
            InputAtom::Conjunction { inner, .. } | InputAtom::Disjunction { inner, .. } => {
                for a in inner {
                    collect_atom(a, out);
                }
            }
            InputAtom::Rule { .. }
            | InputAtom::Predicate { .. }
            | InputAtom::Unification { .. } => {}
        }
    }

    let mut out = BTreeSet::new();
    for (_name, def) in prog.iter_all() {
        match def {
            InputInlineRulesOrFixed::Rules { rules } => {
                for rule in rules {
                    for atom in &rule.body {
                        collect_atom(atom, &mut out);
                    }
                }
            }
            InputInlineRulesOrFixed::Fixed { fixed } => {
                for arg in &fixed.rule_args {
                    match arg {
                        FixedRuleArg::Stored { name, .. }
                        | FixedRuleArg::NamedStored { name, .. } => {
                            out.insert(name.name.clone());
                        }
                        FixedRuleArg::InMem { .. } => {}
                    }
                }
            }
        }
    }
    out
}

impl<S: Storage> Db<S> {
    /// Evaluate a constraint body read-only and return its satisfying rows
    /// — the violation witnesses — sorted and deduped (deterministic at any
    /// thread count). Runs under the session's budget.
    fn eval_constraint_body<T: ReadTx>(
        &self,
        store: &T,
        temp: &TempTx,
        program: InputProgram,
        cur_vld: ValidityTs,
        options: &ScriptOptions,
    ) -> Result<Vec<Tuple>> {
        let (result, _limited, _head, _out_opts) = self.compile_and_eval(
            store,
            temp,
            program,
            cur_vld,
            options,
            // Constraint bodies read the WRITE tx's post-write view;
            // committed-state segments must never serve them.
            crate::engines::segments::Segments::OFF,
        )?;
        let mut rows: Vec<Tuple> = result.all_iter().map(|t| t.into_tuple()).collect();
        rows.sort();
        rows.dedup();
        Ok(rows)
    }

    /// The denial check: evaluate every constraint noted by this
    /// transaction's mutations against the post-write state, in name order.
    /// The first non-empty result aborts the transaction with a typed,
    /// spanned, witnessed [`ConstraintViolation`].
    pub(crate) fn enforce_constraints(
        &self,
        tx: &mut SessionTx<S::WriteTx>,
        cur_vld: ValidityTs,
    ) -> Result<()> {
        if tx.pending_constraints.is_empty() {
            return Ok(());
        }
        let pending = std::mem::take(&mut tx.pending_constraints);
        let fixed = self.fixed_rules();
        let options = tx.options.clone();
        for (name, source) in pending {
            let program = tx.parsed_trigger(&source, &fixed, cur_vld)?;
            // Defensive purity re-check: the catalog row's bytes are a
            // claim, not a proof; a tampered body must not mutate.
            validate_constraint_purity(&name, program.out_opts())?;
            let witnesses = self
                .eval_constraint_body(&tx.store, &tx.temp, program, cur_vld, &options)
                .wrap_err_with(|| format!("while checking integrity constraint '{name}'"))?;
            if !witnesses.is_empty() {
                let total = witnesses.len();
                let shown: Vec<Tuple> = witnesses.into_iter().take(WITNESS_CAP).collect();
                let span = SourceSpan(0, source.len());
                return Err(ConstraintViolation {
                    name: name.to_string(),
                    total,
                    witnesses: shown,
                    body: source,
                    span,
                }
                .into());
            }
        }
        Ok(())
    }

    /// `::constraint create <name> { <body> }`: validate purity, compute the
    /// read-set, refuse if existing data violates the body (with
    /// witnesses), and mirror the [`ConstraintRef`] into the catalog row of
    /// every relation the body reads.
    pub(crate) fn sys_create_constraint(
        &self,
        name: &Symbol,
        source: &str,
        cur_vld: ValidityTs,
        options: &ScriptOptions,
    ) -> Result<NamedRows> {
        let fixed = self.fixed_rules();
        let program =
            parse_script(source, &BTreeMap::new(), &fixed, cur_vld)?.get_single_program()?;
        validate_constraint_purity(&name.name, program.out_opts())?;
        let read_set = stored_read_set(&program);
        if read_set.is_empty() {
            bail!(ConstraintReadsNothing(name.to_string()));
        }
        for rel in &read_set {
            if rel.starts_with('_') {
                bail!(ConstraintOnTempRelation(name.to_string(), rel.to_string()));
            }
        }

        crate::storage::retry::retry_on_conflict(MAX_COMMIT_ATTEMPTS, || {
            let mut tx = SessionTx::new_write(self.storage.write_tx()?, options.clone());

            // Constraint names are one global namespace: scan the catalog.
            for handle in list_relations(&tx.store)? {
                if let Some(c) = handle.constraints.iter().find(|c| c.name == name.name) {
                    bail!(ConstraintNameTaken(
                        c.name.to_string(),
                        handle.name.to_string()
                    ));
                }
            }

            // L4: a constraint current data violates never comes into
            // being. Full-state evaluation inside the creating transaction,
            // under the caller's budget.
            let witnesses = self
                .eval_constraint_body(&tx.store, &tx.temp, program.clone(), cur_vld, options)
                .wrap_err_with(|| {
                    format!("while checking integrity constraint '{name}' over existing data")
                })?;
            if !witnesses.is_empty() {
                let total = witnesses.len();
                let shown: Vec<Tuple> = witnesses.into_iter().take(WITNESS_CAP).collect();
                bail!(ConstraintRejectedOnCreation {
                    name: name.to_string(),
                    total,
                    witnesses: shown,
                    body: source.to_string(),
                    span: SourceSpan(0, source.len()),
                });
            }

            // Attach: the identical spec mirrored onto every relation the
            // body reads, kept name-sorted. Requires the trigger rung of
            // the access ladder on each.
            for rel in &read_set {
                let mut handle = get_relation(&tx.store, rel)?;
                if handle.access_level < AccessLevel::Protected {
                    bail!(InsufficientAccessLevel(
                        handle.name.to_string(),
                        "create constraint".to_string(),
                        handle.access_level
                    ));
                }
                handle.constraints.push(ConstraintRef {
                    name: name.name.clone(),
                    source: source.to_string(),
                });
                handle.constraints.sort_by(|a, b| a.name.cmp(&b.name));
                write_relation_row(&mut tx.store, &handle)?;
            }
            tx.store.commit()?;
            Ok(NamedRows::new(
                vec!["status".to_string()],
                vec![Tuple::from_vec(vec![DataValue::from("OK")])],
            ))
        })
    }

    /// `::constraint drop <name>`: strip the constraint from every catalog
    /// row carrying it. Dropping a name nothing carries is a typed refusal.
    ///
    /// Removing an invariant is gated by the same access rung that created it
    /// (`AccessLevel::Protected` on every carrying relation): a constraint
    /// proved at construction must not be detachable below the rung that
    /// established it, or `::set_access_level r read_only` would become a
    /// backdoor to lifting a denial that the relation's writers still rely on.
    pub(crate) fn sys_remove_constraint(&self, name: &Symbol) -> Result<NamedRows> {
        crate::storage::retry::retry_on_conflict(MAX_COMMIT_ATTEMPTS, || {
            let mut tx = SessionTx::new_write(self.storage.write_tx()?, ScriptOptions::default());
            let mut found = false;
            for mut handle in list_relations(&tx.store)? {
                let before = handle.constraints.len();
                if handle.constraints.iter().any(|c| c.name == name.name)
                    && handle.access_level < AccessLevel::Protected
                {
                    bail!(InsufficientAccessLevel(
                        handle.name.to_string(),
                        "drop constraint".to_string(),
                        handle.access_level
                    ));
                }
                handle.constraints.retain(|c| c.name != name.name);
                if handle.constraints.len() != before {
                    found = true;
                    write_relation_row(&mut tx.store, &handle)?;
                }
            }
            if !found {
                bail!(NoSuchConstraint(name.to_string()));
            }
            tx.store.commit()?;
            Ok(NamedRows::new(
                vec!["status".to_string()],
                vec![Tuple::from_vec(vec![DataValue::from("OK")])],
            ))
        })
    }

    /// `::constraint list`: every (constraint, attached relation, body)
    /// triple, in catalog order — a mirrored constraint appears once per
    /// relation it reads.
    pub(crate) fn sys_list_constraints(&self) -> Result<NamedRows> {
        let tx = SessionTx::new_read(self.storage.read_tx()?, ScriptOptions::default());
        let mut rows = vec![];
        for handle in list_relations(&tx.store)? {
            for c in &handle.constraints {
                rows.push(Tuple::from_vec(vec![
                    DataValue::from(c.name.as_str()),
                    DataValue::from(handle.name.as_str()),
                    DataValue::from(c.source.as_str()),
                ]));
            }
        }
        Ok(NamedRows::new(
            vec!["name".into(), "relation".into(), "source".into()],
            rows,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::relation::RelationHasConstraints;
    use crate::storage::fjall::new_fjall_storage;
    use crate::storage::sim::SimStorage;

    fn no_params() -> BTreeMap<String, DataValue> {
        BTreeMap::new()
    }

    fn ints(nr: &NamedRows) -> Vec<Vec<i64>> {
        let mut out: Vec<Vec<i64>> = nr
            .rows
            .iter()
            .map(|r| r.iter().map(|v| v.get_int().expect("int")).collect())
            .collect();
        out.sort();
        out
    }

    /// CHECK shape end to end: a violating insert is denied with the
    /// violating row as witness, the whole transaction rolls back, and a
    /// satisfying insert commits. THIS is the enforcement-path tripwire:
    /// disabling the denial check in `enforce_constraints` (or the
    /// `note_constraints` collection, or the `execute_single` hook) makes
    /// the `expect_err` here pass a violating commit and the test fail.
    #[test]
    fn check_constraint_denies_violating_insert_with_witnesses() {
        let db = Db::new(SimStorage::new(21)).unwrap();
        db.run_script("?[k, v] <- [[1, 5]] :create scores {k => v}", no_params())
            .expect("create");
        db.run_script(
            "::constraint create nonneg { ?[k, v] := *scores[k, v], v < 0 }",
            no_params(),
        )
        .expect("constraint creation over clean data");

        // A satisfying insert commits.
        db.run_script("?[k, v] <- [[2, 7]] :put scores {k, v}", no_params())
            .expect("satisfying insert commits");

        // A violating insert is denied, typed and witnessed.
        let err = db
            .run_script(
                "?[k, v] <- [[3, -4], [4, 9]] :put scores {k, v}",
                no_params(),
            )
            .expect_err("violating insert must be denied");
        let viol = err
            .downcast_ref::<ConstraintViolation>()
            .expect("typed ConstraintViolation");
        assert_eq!(viol.name, "nonneg");
        assert_eq!(viol.total, 1);
        let want: Vec<Tuple> = vec![Tuple::from_vec(vec![
            DataValue::from(3),
            DataValue::from(-4),
        ])];
        assert_eq!(
            viol.witnesses, want,
            "the witness is exactly the violating row, post-write"
        );

        // The abort was whole: the co-inserted GOOD row [4, 9] is gone too.
        let out = db
            .run_script("?[k, v] := *scores[k, v]", no_params())
            .expect("scan");
        assert_eq!(ints(&out), vec![vec![1, 5], vec![2, 7]]);
    }

    /// FK shape, both directions: inserting a child without its parent is
    /// denied (child-side attachment), and deleting a parent that still has
    /// children is denied (parent-side attachment of the SAME constraint).
    #[test]
    fn fk_constraint_fires_on_child_insert_and_parent_delete() {
        let db = Db::new(SimStorage::new(22)).unwrap();
        db.run_script("?[id] <- [[1]] :create parent {id}", no_params())
            .expect("create parent");
        db.run_script("?[id, fk] <- [] :create child {id => fk}", no_params())
            .expect("create child");
        db.run_script(
            "::constraint create fk_child_parent { ?[fk] := *child{fk}, not *parent{id: fk} }",
            no_params(),
        )
        .expect("create fk constraint");

        // Both relations carry the mirrored spec.
        let listed = db.run_script("::constraint list", no_params()).unwrap();
        let attached: Vec<String> = listed
            .rows
            .iter()
            .map(|r| format!("{:?}|{:?}", r[0], r[1]))
            .collect();
        assert_eq!(listed.rows.len(), 2, "mirrored onto child AND parent");
        assert!(attached.iter().all(|s| s.contains("fk_child_parent")));

        // Child referencing an existing parent: commits.
        db.run_script("?[id, fk] <- [[10, 1]] :put child {id, fk}", no_params())
            .expect("valid child commits");
        // Child referencing a missing parent: denied with the dangling key.
        let err = db
            .run_script("?[id, fk] <- [[11, 2]] :put child {id, fk}", no_params())
            .expect_err("orphan child denied");
        let viol = err.downcast_ref::<ConstraintViolation>().expect("typed");
        let want: Vec<Tuple> = vec![Tuple::from_vec(vec![DataValue::from(2)])];
        assert_eq!(viol.witnesses, want);

        // Deleting the parent while a child references it: denied through
        // the parent-side attachment.
        let err = db
            .run_script("?[id] <- [[1]] :rm parent {id}", no_params())
            .expect_err("parent delete with live children denied");
        assert!(err.downcast_ref::<ConstraintViolation>().is_some());

        // Remove the child first, then the parent delete commits.
        db.run_script("?[id] <- [[10]] :rm child {id}", no_params())
            .expect("child removal");
        db.run_script("?[id] <- [[1]] :rm parent {id}", no_params())
            .expect("parent delete commits once no child refers to it");
    }

    /// L4: creating a constraint over already-violating data refuses
    /// creation with the offending rows; after repairing, creation
    /// succeeds; dropping makes previously denied writes commit again.
    #[test]
    fn creation_over_violating_data_is_refused_with_witnesses() {
        let db = Db::new(SimStorage::new(23)).unwrap();
        db.run_script(
            "?[k, v] <- [[1, -9], [2, 3]] :create scores {k => v}",
            no_params(),
        )
        .expect("create with a pre-existing violation");

        let err = db
            .run_script(
                "::constraint create nonneg { ?[k, v] := *scores[k, v], v < 0 }",
                no_params(),
            )
            .expect_err("creation over violating data must refuse");
        let rej = err
            .downcast_ref::<ConstraintRejectedOnCreation>()
            .expect("typed creation rejection");
        assert_eq!(rej.total, 1);
        let want: Vec<Tuple> = vec![Tuple::from_vec(vec![
            DataValue::from(1),
            DataValue::from(-9),
        ])];
        assert_eq!(rej.witnesses, want);

        // Nothing was attached by the refused creation.
        let listed = db.run_script("::constraint list", no_params()).unwrap();
        assert!(listed.rows.is_empty());

        // Repair, create, and the constraint now enforces.
        db.run_script("?[k, v] <- [[1, 9]] :put scores {k, v}", no_params())
            .expect("repair");
        db.run_script(
            "::constraint create nonneg { ?[k, v] := *scores[k, v], v < 0 }",
            no_params(),
        )
        .expect("creation over clean data");
        db.run_script("?[k, v] <- [[3, -1]] :put scores {k, v}", no_params())
            .expect_err("now enforced");

        // Dropping the constraint lifts the denial; dropping again refuses.
        db.run_script("::constraint drop nonneg", no_params())
            .expect("drop");
        db.run_script("?[k, v] <- [[3, -1]] :put scores {k, v}", no_params())
            .expect("enforcement gone after drop");
        let err = db
            .run_script("::constraint drop nonneg", no_params())
            .expect_err("double drop refused");
        assert!(err.downcast_ref::<NoSuchConstraint>().is_some());
    }

    /// Constraint × trigger composition: a trigger's writes are subject to
    /// constraints, and the denial rolls back the user's write AND the
    /// trigger's write together.
    #[test]
    fn trigger_writes_are_constrained_and_abort_is_atomic() {
        let db = Db::new(SimStorage::new(24)).unwrap();
        db.run_script("?[x] <- [] :create a {x}", no_params())
            .expect("create a");
        db.run_script("?[y] <- [] :create b {y}", no_params())
            .expect("create b");
        db.run_script(
            "::set_triggers a on put { ?[y] := _new[x], y = x :put b {y} }",
            no_params(),
        )
        .expect("trigger a→b");
        db.run_script(
            "::constraint create small_b { ?[y] := *b[y], y > 10 }",
            no_params(),
        )
        .expect("constraint on b");

        // A put whose trigger output satisfies the constraint commits.
        db.run_script("?[x] <- [[5]] :put a {x}", no_params())
            .expect("satisfying cascade commits");
        assert_eq!(
            ints(&db.run_script("?[y] := *b[y]", no_params()).unwrap()),
            vec![vec![5]]
        );

        // A put whose TRIGGER write violates the constraint on b is denied
        // whole: neither the a-row nor the b-row survives.
        let err = db
            .run_script("?[x] <- [[50]] :put a {x}", no_params())
            .expect_err("trigger write violates b's constraint");
        let viol = err.downcast_ref::<ConstraintViolation>().expect("typed");
        assert_eq!(viol.name, "small_b");
        let want: Vec<Tuple> = vec![Tuple::from_vec(vec![DataValue::from(50)])];
        assert_eq!(viol.witnesses, want);
        assert_eq!(
            ints(&db.run_script("?[x] := *a[x]", no_params()).unwrap()),
            vec![vec![5]],
            "the user's own write rolled back with the trigger's"
        );
        assert_eq!(
            ints(&db.run_script("?[y] := *b[y]", no_params()).unwrap()),
            vec![vec![5]],
            "the trigger's write rolled back too"
        );
    }

    /// A constraint whose body exceeds the session budget is a typed
    /// refusal naming the constraint — never a hang — and the write it was
    /// checking rolls back.
    #[test]
    fn constraint_exceeding_budget_is_a_typed_refusal() {
        let db = Db::new(SimStorage::new(25)).unwrap();
        db.run_script(
            "?[a, b] <- [[1, 2], [2, 3], [3, 4], [4, 2]] :create edge {a, b}",
            no_params(),
        )
        .expect("create edges");
        // The body computes the full transitive closure (12 derived path
        // tuples) and then filters to nothing: satisfiable never, expensive
        // always.
        db.run_script(
            "::constraint create acyclic_probe { \
               path[a, b] := *edge[a, b] \
               path[a, b] := *edge[a, c], path[c, b] \
               ?[a, b] := path[a, b], a < 0 \
             }",
            no_params(),
        )
        .expect("constraint creation under the default (generous) budget");

        // Under a tiny derived-tuple ceiling the insert itself is cheap but
        // the constraint's closure is not: typed refusal, transaction
        // aborted.
        let opts = ScriptOptions {
            derived_tuple_ceiling: Some(6),
            ..Default::default()
        };
        let err = db
            .run_script_with("?[a, b] <- [[9, 9]] :put edge {a, b}", no_params(), opts)
            .expect_err("constraint eval must refuse under the ceiling");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("acyclic_probe"),
            "the refusal names the constraint: {msg}"
        );
        assert!(
            msg.contains("budget") || msg.contains("ceiling") || msg.contains("derived"),
            "the refusal is the typed budget refusal: {msg}"
        );
        // The insert rolled back with the refusal.
        let out = db
            .run_script("?[a, b] := *edge[a, b]", no_params())
            .unwrap();
        assert_eq!(out.rows.len(), 4, "the guarded insert rolled back");

        // The same insert under the default budget commits.
        db.run_script("?[a, b] <- [[9, 9]] :put edge {a, b}", no_params())
            .expect("commits under the default budget");
    }

    /// Determinism: the same violation yields byte-identical witnesses —
    /// sorted, capped at [`WITNESS_CAP`] with the full total reported — at
    /// every rayon thread count and on both storage backends.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn witnesses_are_deterministic_across_thread_counts_and_backends() {
        fn violate<S: Storage>(db: &Db<S>) -> (String, usize, Vec<Tuple>) {
            db.run_script("?[k, v] <- [[0, 0]] :create scores {k => v}", no_params())
                .expect("create");
            db.run_script(
                "::constraint create nonneg { ?[k, v] := *scores[k, v], v < 0 }",
                no_params(),
            )
            .expect("constraint");
            let rows: Vec<String> = (1..=20).map(|i| format!("[{i}, {}]", -i)).collect();
            let script = format!("?[k, v] <- [{}] :put scores {{k, v}}", rows.join(", "));
            let err = db
                .run_script(&script, no_params())
                .expect_err("20 violations denied");
            let viol = err.downcast_ref::<ConstraintViolation>().expect("typed");
            (viol.name.clone(), viol.total, viol.witnesses.clone())
        }

        fn at_thread_count<T: Send>(threads: usize, f: impl FnOnce() -> T + Send) -> T {
            rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .expect("thread pool")
                .install(f)
        }

        let baseline = at_thread_count(1, || violate(&Db::new(SimStorage::new(7)).unwrap()));
        assert_eq!(baseline.1, 20, "full violation count reported");
        assert_eq!(baseline.2.len(), WITNESS_CAP, "witness list capped");
        // Sorted ⇒ the cap keeps the smallest keys 1..=8.
        assert_eq!(
            baseline.2.first().unwrap(),
            &Tuple::from_vec(vec![DataValue::from(1), DataValue::from(-1)])
        );

        for threads in [2, 4] {
            let got = at_thread_count(threads, || violate(&Db::new(SimStorage::new(7)).unwrap()));
            assert_eq!(got, baseline, "witnesses differ at {threads} threads");
        }
        let dir = tempfile::tempdir().unwrap();
        let on_fjall = at_thread_count(2, || {
            violate(&Db::new(new_fjall_storage(dir.path()).unwrap()).unwrap())
        });
        assert_eq!(on_fjall, baseline, "witnesses differ across backends");
    }

    /// The creation-time refusals: impure bodies, empty read-sets, temp
    /// relations, duplicate names, and missing referenced relations are all
    /// typed refusals, and none of them attaches anything.
    #[test]
    fn creation_refusals_are_typed_and_attach_nothing() {
        let db = Db::new(SimStorage::new(26)).unwrap();
        db.run_script("?[k] <- [[1]] :create r {k}", no_params())
            .expect("create r");

        // Mutating body.
        let err = db
            .run_script(
                "::constraint create bad1 { ?[k] := *r[k] :put r {k} }",
                no_params(),
            )
            .expect_err("mutating body refused");
        assert!(err.downcast_ref::<ConstraintNotPure>().is_some());

        // `:timeout` body: refused, NOT panicked. An unbounded `:timeout`
        // would overflow `Duration::from_secs_f64` inside creation-time eval
        // (hostile-review finding: user-input panic on the general query
        // path); the purity gate refuses it before it can reach the budget.
        let err = db
            .run_script(
                "::constraint create bad_to { ?[k] := *r[k], k < 0 :timeout 1e300 }",
                no_params(),
            )
            .expect_err(":timeout body refused, not panicked");
        assert!(err.downcast_ref::<ConstraintNotPure>().is_some());

        // `:sleep` body: refused for the same purity reason.
        let err = db
            .run_script(
                "::constraint create bad_sl { ?[k] := *r[k], k < 0 :sleep 1 }",
                no_params(),
            )
            .expect_err(":sleep body refused");
        assert!(err.downcast_ref::<ConstraintNotPure>().is_some());

        // :limit body (a `:limit 0` would silently hide every violation).
        let err = db
            .run_script(
                "::constraint create bad2 { ?[k] := *r[k], k < 0 :limit 0 }",
                no_params(),
            )
            .expect_err(":limit body refused");
        assert!(err.downcast_ref::<ConstraintNotPure>().is_some());

        // :assert body.
        let err = db
            .run_script(
                "::constraint create bad3 { ?[k] := *r[k], k < 0 :assert none }",
                no_params(),
            )
            .expect_err(":assert body refused");
        assert!(err.downcast_ref::<ConstraintNotPure>().is_some());

        // Reads no stored relation.
        let err = db
            .run_script("::constraint create bad4 { ?[x] := x = 1 }", no_params())
            .expect_err("read-set-free body refused");
        assert!(err.downcast_ref::<ConstraintReadsNothing>().is_some());

        // Reads a temp relation.
        let err = db
            .run_script(
                "::constraint create bad5 { ?[k] := *_scratch[k] }",
                no_params(),
            )
            .expect_err("temp read-set refused");
        assert!(err.downcast_ref::<ConstraintOnTempRelation>().is_some());

        // Reads a relation that does not exist.
        assert!(
            db.run_script(
                "::constraint create bad6 { ?[k] := *nonexistent[k], k < 0 }",
                no_params(),
            )
            .is_err(),
            "missing relation in the read-set refused"
        );

        // Duplicate name.
        db.run_script(
            "::constraint create dup { ?[k] := *r[k], k < 0 }",
            no_params(),
        )
        .expect("first creation");
        let err = db
            .run_script(
                "::constraint create dup { ?[k] := *r[k], k > 100 }",
                no_params(),
            )
            .expect_err("duplicate name refused");
        assert!(err.downcast_ref::<ConstraintNameTaken>().is_some());

        // Exactly one attachment survives all the refusals.
        let listed = db.run_script("::constraint list", no_params()).unwrap();
        assert_eq!(listed.rows.len(), 1);
    }

    /// Destroying, renaming, or `:replace`-ing a relation that participates
    /// in a constraint is refused until the constraint is dropped —
    /// otherwise sibling writes would fail forever on a dangling reference.
    #[test]
    fn destroy_rename_replace_refused_while_constrained() {
        let db = Db::new(SimStorage::new(27)).unwrap();
        db.run_script("?[id] <- [[1]] :create parent {id}", no_params())
            .expect("create parent");
        db.run_script("?[id, fk] <- [] :create child {id => fk}", no_params())
            .expect("create child");
        db.run_script(
            "::constraint create fk { ?[fk] := *child{fk}, not *parent{id: fk} }",
            no_params(),
        )
        .expect("create constraint");

        // The PARENT (a read-set participant, not just the child) is held.
        let err = db
            .run_script("::remove parent", no_params())
            .expect_err("remove refused");
        assert!(err.downcast_ref::<RelationHasConstraints>().is_some());
        let err = db
            .run_script("::rename parent -> progenitor", no_params())
            .expect_err("rename refused");
        assert!(err.downcast_ref::<RelationHasConstraints>().is_some());
        let err = db
            .run_script("?[id] <- [[2]] :replace parent {id}", no_params())
            .expect_err(":replace refused");
        assert!(err.downcast_ref::<RelationHasConstraints>().is_some());

        // Drop the constraint and the catalog ops proceed.
        db.run_script("::constraint drop fk", no_params())
            .expect("drop constraint");
        db.run_script("?[id] <- [[2]] :replace parent {id}", no_params())
            .expect(":replace after drop");
        db.run_script("::remove parent", no_params())
            .expect("remove after drop");
    }

    /// Removing a constraint is gated by the same access rung that created it
    /// (`Protected`): lowering a carrying relation to `read_only` must NOT
    /// become a backdoor for dropping a denial its writers still depend on.
    /// (Hostile-review finding: the drop path once ran with no access check.)
    #[test]
    fn drop_requires_the_same_access_rung_as_create() {
        let db = Db::new(SimStorage::new(30)).unwrap();
        db.run_script("?[k, v] <- [[1, 5]] :create scores {k => v}", no_params())
            .expect("create");
        db.run_script(
            "::constraint create nonneg { ?[k, v] := *scores[k, v], v < 0 }",
            no_params(),
        )
        .expect("constraint creation (relation is Normal, above Protected)");

        // Drop it to read_only — below the Protected rung the constraint was
        // established at.
        db.run_script("::access_level read_only scores", no_params())
            .expect("lower access");

        // The denial still stands: a violating write is refused.
        let err = db
            .run_script("?[k, v] <- [[2, -1]] :put scores {k, v}", no_params())
            .expect_err("write refused (read_only)");
        // read_only blocks the write at the access ladder before the denial,
        // but the constraint must remain undroppable at this rung.
        assert!(err.downcast_ref::<ConstraintViolation>().is_none());

        // Dropping the constraint at read_only is refused: the ladder holds.
        let err = db
            .run_script("::constraint drop nonneg", no_params())
            .expect_err("drop refused below Protected");
        assert!(
            err.downcast_ref::<InsufficientAccessLevel>().is_some(),
            "drop below Protected must be a typed access refusal, got: {err:?}"
        );
        // And nothing was stripped — the constraint is still listed.
        let listed = db.run_script("::constraint list", no_params()).unwrap();
        assert_eq!(listed.rows.len(), 1, "the refused drop detached nothing");

        // Restore the rung, then the drop proceeds — the invariant can only
        // be removed at (or above) the rung that created it.
        db.run_script("::access_level normal scores", no_params())
            .expect("restore access");
        db.run_script("::constraint drop nonneg", no_params())
            .expect("drop at Protected+ succeeds");
        assert!(
            db.run_script("::constraint list", no_params())
                .unwrap()
                .rows
                .is_empty()
        );
    }

    /// The corrected cascade shape: triggers cascade past depth 1 (an
    /// a→b→c chain delivers to c), and a trigger cycle is a typed
    /// depth-ceiling refusal that aborts the transaction whole — never a
    /// silent stop, never an unbounded loop.
    #[test]
    fn trigger_cascade_runs_deep_and_cycles_hit_the_typed_ceiling() {
        let db = Db::new(SimStorage::new(28)).unwrap();
        for rel in ["a", "b", "c"] {
            db.run_script(&format!("?[x] <- [] :create {rel} {{x}}"), no_params())
                .expect("create");
        }
        db.run_script(
            "::set_triggers a on put { ?[x] := _new[x] :put b {x} }",
            no_params(),
        )
        .expect("trigger a→b");
        db.run_script(
            "::set_triggers b on put { ?[x] := _new[x] :put c {x} }",
            no_params(),
        )
        .expect("trigger b→c");

        db.run_script("?[x] <- [[7]] :put a {x}", no_params())
            .expect("cascading put");
        assert_eq!(
            ints(&db.run_script("?[x] := *c[x]", no_params()).unwrap()),
            vec![vec![7]],
            "the cascade reached depth 2 (the session draft's silent \
             depth-1 cut would have dropped this)"
        );

        // A self-cycle: put s → trigger puts s → … refused at the ceiling,
        // and the transaction aborts whole (no s-row survives).
        db.run_script("?[x] <- [] :create s {x}", no_params())
            .expect("create s");
        db.run_script(
            "::set_triggers s on put { ?[x] := _new[x] :put s {x} }",
            no_params(),
        )
        .expect("cyclic trigger");
        let err = db
            .run_script("?[x] <- [[1]] :put s {x}", no_params())
            .expect_err("cycle must hit the typed ceiling");
        // Trigger recursion attaches the trigger source as source_code at
        // every unwind level, so the concrete type is behind miette
        // adapters; the diagnostic code is the stable identity.
        let msg = format!("{err:?}");
        assert!(
            msg.contains("tx::trigger_cascade_too_deep")
                && msg.contains("exceeded the depth ceiling of 32"),
            "typed ceiling refusal, got: {msg}"
        );
        assert_eq!(
            db.run_script("?[x] := *s[x]", no_params())
                .unwrap()
                .rows
                .len(),
            0,
            "the aborted cascade kept nothing"
        );
    }

    /// Secondary uniqueness as a denial rule: two distinct dependents for
    /// one logical key are denied.
    #[test]
    fn uniqueness_constraint_shape() {
        let db = Db::new(SimStorage::new(29)).unwrap();
        // email is a dependent column; k is the primary key.
        db.run_script(
            "?[k, email] <- [[1, 'a@x.com']] :create users {k => email}",
            no_params(),
        )
        .expect("create");
        db.run_script(
            "::constraint create unique_email { \
               ?[email] := *users[k1, email], *users[k2, email], k1 != k2 \
             }",
            no_params(),
        )
        .expect("create uniqueness constraint");

        db.run_script(
            "?[k, email] <- [[2, 'b@x.com']] :put users {k, email}",
            no_params(),
        )
        .expect("distinct email commits");
        let err = db
            .run_script(
                "?[k, email] <- [[3, 'a@x.com']] :put users {k, email}",
                no_params(),
            )
            .expect_err("duplicate email denied");
        let viol = err.downcast_ref::<ConstraintViolation>().expect("typed");
        let want: Vec<Tuple> = vec![Tuple::from_vec(vec![DataValue::from("a@x.com")])];
        assert_eq!(viol.witnesses, want);
    }
}
