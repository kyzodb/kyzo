/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (`runtime/db.rs` + `runtime/transact.rs`, MPL-2.0), re-architected for the
 * KyzoDB kernel and session model (story #3):
 *
 * - **Session species.** A session is a [`SessionTx<T>`] owning its backend
 *   transaction `T` and a private scratch store. Mutation lives on
 *   `T: WriteTx` only, so writing through a read session does not compile —
 *   the read/write distinction is a type, not a convention. No `'s`
 *   transaction lifetime is threaded through the engine; the session owns
 *   its transaction and is `Send`.
 * - **Conflict retry.** Every write commit is wrapped by
 *   [`crate::storage::retry::retry_on_conflict`]: a `ConflictError` at commit
 *   rebuilds a fresh transaction AND a fresh callback collector and replays
 *   the query. The collector is plain data collected during the attempt and
 *   delivered only after a successful commit, so a conflicted attempt leaks
 *   no phantom events.
 * - **The cleanups machinery is gone.** Upstream deferred key-range deletes
 *   to a post-logic, pre-commit `del_range_from_persisted` pass. The kernel's
 *   `del_range` deletes inside the transaction, so `:replace`/`::remove` are
 *   atomic with the query and roll back on abort. Mutation returns nothing
 *   but the rows.
 * - **Budget is required by parameter.** Evaluation takes a
 *   [`crate::query::eval::Budget`] built from the query's options and the
 *   caller's [`ScriptOptions`]: a deterministic epoch ceiling checked at
 *   epoch barriers, an optional deterministic derived-tuple ceiling, and an
 *   optional wall-clock deadline. There is no cooperative-poison thread and
 *   nothing sleeps to enforce a limit.
 * - **The catalog is typed.** Relation rows are addressed through
 *   `runtime/relation.rs`'s [`SystemKey`], and `current_validity()` is
 *   fallible and threaded as `?`.
 *
 * - **Fixed rules run.** Registration (register/unregister/re-exports) and
 *   evaluation are both wired: a query that APPLIES a fixed rule builds the
 *   `FixedRuleEval` adapter ([`crate::query::normalize::SessionFixedRule`])
 *   that bridges `MagicFixedRuleApply` to `FixedRule::run`, sharing the
 *   budget's kill flag as the rule's `CancelFlag`. This includes the
 *   `Constant` rule behind every `<- [[…]]` inline datum.
 *
 * INTERIM (named, not smoothed over):
 * - Index-operator system ops (`::index`, `::hnsw`, `::fts`, `::lsh`) hit a
 *   typed refusal until the operator tier lands; catalog ops are complete.
 * - The imperative script genus (`Script::Imperative`) is refused; the query
 *   and system genera are executed. `::explain` and `::running`/`::kill` are
 *   likewise deferred (typed refusals / empty results).
 */

//! The database entrypoint: from a script string to result rows.
//!
//! [`Db`] is the process-wide handle — storage plus the fixed-rule and
//! event-callback registries. [`Db::run_script`] parses a script and runs it:
//! a query compiles (normalize → stratify → magic-sets → relational-algebra
//! plan) and evaluates semi-naively over the session's transaction, a
//! mutation additionally writes its result set back through the mutation
//! pipeline, and a system op reads or edits the catalog. The result is a
//! [`NamedRows`].

use std::collections::{BTreeMap, BTreeSet};
use std::num::{NonZeroU32, NonZeroU64};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use miette::{Diagnostic, Result, bail};
use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

use crate::data::program::{
    InputProgram, QueryAssertion, QueryOutOptions, RelationOp, ReturnMutation,
};
use crate::data::span::SourceSpan;
use crate::data::symb::Symbol;
use crate::data::tuple::Tuple;
use crate::data::value::{AsOf, DataValue, ValidityTs, current_validity};
use crate::fixed_rule::{CancelFlag, DEFAULT_FIXED_RULES, FixedRule, NamedRows};
use crate::parse::sys::{AccessLevel as ParseAccessLevel, SysOp};
use crate::parse::{Script, parse_script};
use crate::query::compile::stratified_magic_compile;
use crate::query::eval::{Budget, RowLimit, stratified_evaluate};
use crate::query::levels::EpochStore;
use crate::query::normalize::{SessionFixedRule, SessionNormalizer, SessionView};
use crate::query::sort::sort_and_collect;
use crate::runtime::callback::{CallbackCollector, EventCallbackRegistry};
use crate::runtime::relation::{
    AccessLevel, KeyspaceKind, RelationHandle, create_relation, describe_relation,
    destroy_relation, get_relation, list_relations, rename_relation, set_access_level,
    set_relation_triggers, write_relation_row,
};
use crate::storage::temp::TempTx;
use crate::storage::{ReadTx, Storage, WriteTx};

/// The deterministic default ceiling on evaluation epochs (semi-naive
/// iterations). High enough for real recursion over finite data; bounds a
/// runaway fixpoint into a typed refusal at an epoch barrier rather than an
/// unbounded loop. Overridable per script through [`ScriptOptions`].
const DEFAULT_EPOCH_CEILING: u32 = 1_000_000;

/// How many times a write commit replays on a typed [`ConflictError`]
/// before giving up. Reads never conflict. This is a liveness backstop
/// against pathological contention, not a tuning knob: with the retry
/// tier's capped exponential backoff, exhausting it means roughly eight
/// seconds of continuous same-fact races — at 32 it was reachable by
/// three writers under a loaded machine, which is contention working,
/// not failing.
///
/// [`ConflictError`]: crate::storage::ConflictError
const MAX_COMMIT_ATTEMPTS: usize = 128;

/// A script asked for the imperative genus (`?[…] <- …` control flow), which
/// the session tier executes for queries and system ops but not yet for
/// imperative blocks.
#[derive(Debug, Error, Diagnostic)]
#[error("imperative scripts are not executed yet")]
#[diagnostic(code(db::imperative_not_wired))]
pub(crate) struct ImperativeNotWired;

/// A store op targeting a temp relation (`_`-prefixed). In this tier the
/// session — and with it the temp store — lives exactly as long as one
/// script, so a temp write could never be observed; without this refusal
/// the read path would silently drop the mutation (review finding F2).
/// Lands for real with multi-script sessions.
#[derive(Debug, Error, Diagnostic)]
#[error("temp relation '{0}' cannot be stored to yet: sessions do not outlive a script")]
#[diagnostic(code(db::temp_relation_not_reachable))]
#[diagnostic(help(
    "temp relations (`_`-prefixed) become writable when multi-script \
     sessions land; store to a named relation instead"
))]
pub(crate) struct TempRelationNotReachableError(
    pub(crate) String,
    #[label] pub(crate) crate::data::span::SourceSpan,
);

/// A system op needs the index-operator tier (HNSW / FTS / LSH), which has
/// not landed. Catalog system ops are complete.
#[derive(Debug, Error, Diagnostic)]
#[error("system op '{0}' needs the index-operator tier, which has not landed")]
#[diagnostic(code(db::index_op_not_landed))]
pub(crate) struct IndexOpNotLanded(pub(crate) &'static str);

/// A `:assert none` / `:assert some` query option was violated.
#[derive(Debug, Error, Diagnostic)]
#[error("{0}")]
#[diagnostic(code(db::assertion_failure))]
pub(crate) struct QueryAssertionFailure(String, #[label] crate::data::span::SourceSpan);

/// Registering a fixed rule under a name already taken.
#[derive(Debug, Error, Diagnostic)]
#[error("cannot register fixed rule '{0}': the name is already taken")]
#[diagnostic(code(db::fixed_rule_name_conflict))]
pub(crate) struct FixedRuleNameConflict(pub(crate) String);

/// A mutation named an output relation whose precondition the op requires:
/// `:create` on an existing relation, or a non-create/replace op on a
/// missing one.
#[derive(Debug, Error, Diagnostic)]
#[error("{0}")]
#[diagnostic(code(db::store_relation_precondition))]
pub(crate) struct StoreRelationPrecondition(String);

/// A `:timeout` (or caller-supplied deadline) that cannot become a
/// [`Duration`]: negative, non-finite, or too large to fit. The parser only
/// bounds `:timeout` by `> 0`, so this is the last line of defense before
/// `Duration::from_secs_f64` would panic.
#[derive(Debug, Error, Diagnostic)]
#[error(
    "timeout of {0} seconds is not usable: it must be a finite, non-negative number of seconds that fits in a Duration"
)]
#[diagnostic(code(db::invalid_timeout))]
pub(crate) struct InvalidTimeout(pub(crate) f64);

/// The scan ceiling for `::merkle_root` when the caller sets no
/// derived-tuple ceiling: 2^32 key-value pairs. Large enough for any store
/// this engine has met, small enough that no scan is unbounded.
const DEFAULT_MERKLE_SCAN_CEILING: NonZeroU64 = NonZeroU64::new(1 << 32).unwrap();

/// Per-script evaluation controls. Default is "run to the fixpoint within the
/// deterministic epoch ceiling, no deadline". These are the knobs that turn
/// a budget into a refusal; they are deterministic (epoch/derived-tuple
/// ceilings) except the wall-clock `timeout`.
#[derive(Clone, Debug, Default)]
pub struct ScriptOptions {
    /// Override the epoch (semi-naive iteration) ceiling. `None` uses
    /// [`DEFAULT_EPOCH_CEILING`].
    pub epoch_ceiling: Option<u32>,
    /// A deterministic ceiling on the number of derived tuples. `None` is
    /// unbounded. Refusal is exact and reproducible.
    pub derived_tuple_ceiling: Option<u64>,
    /// A wall-clock deadline in seconds. `None` is no deadline. The query's
    /// own `:timeout` option, if smaller, wins.
    pub timeout_secs: Option<f64>,
}

/// One database: a storage backend plus the process-wide registries.
///
/// Cloning a `Db` shares the same storage and registries (the registries are
/// behind `Arc`), so callbacks and fixed rules registered on one clone are
/// visible on the others — the handle is a shared view of one universe.
pub struct Db<S> {
    pub(crate) storage: S,
    pub(crate) segments: Arc<crate::engines::segments::SegmentEngine>,
    pub(crate) fixed_rules: Arc<RwLock<BTreeMap<String, Arc<dyn FixedRule>>>>,
    pub(crate) event_callbacks: Arc<RwLock<EventCallbackRegistry>>,
    pub(crate) callback_count: Arc<AtomicU32>,
}

impl<S: Clone> Clone for Db<S> {
    fn clone(&self) -> Self {
        Self {
            storage: self.storage.clone(),
            segments: self.segments.clone(),
            fixed_rules: self.fixed_rules.clone(),
            event_callbacks: self.event_callbacks.clone(),
            callback_count: self.callback_count.clone(),
        }
    }
}

impl<S: Storage> Db<S> {
    /// Open a database over the given storage backend, seeding the fixed-rule
    /// registry with the built-ins.
    pub fn new(storage: S) -> Result<Self> {
        let fixed_rules = DEFAULT_FIXED_RULES.clone();
        Ok(Self {
            storage,
            segments: Arc::new(crate::engines::segments::SegmentEngine::default()),
            fixed_rules: Arc::new(RwLock::new(fixed_rules)),
            event_callbacks: Arc::new(RwLock::new(EventCallbackRegistry::default())),
            callback_count: Arc::new(AtomicU32::new(0)),
        })
    }

    /// A snapshot of the fixed-rule registry: the built-ins plus every
    /// user-registered rule. Handed to the parser (which resolves fixed-rule
    /// names) and to the mutation pipeline (for trigger parsing).
    pub(crate) fn fixed_rules(&self) -> BTreeMap<String, Arc<dyn FixedRule>> {
        self.fixed_rules
            .read()
            .expect("fixed-rule registry poisoned")
            .clone()
    }

    /// Register a custom fixed rule under `name`. Errors if the name is taken
    /// (including by a built-in). The rule becomes usable in every session of
    /// this `Db` (and its clones).
    pub fn register_fixed_rule(&self, name: String, rule: impl FixedRule + 'static) -> Result<()> {
        let mut registry = self
            .fixed_rules
            .write()
            .expect("fixed-rule registry poisoned");
        if registry.contains_key(&name) {
            bail!(FixedRuleNameConflict(name));
        }
        registry.insert(name, Arc::from(Box::new(rule) as Box<dyn FixedRule>));
        Ok(())
    }

    /// Register a fixed rule from an already-boxed `Arc<dyn FixedRule>` (for
    /// callers holding trait objects). Same name-conflict contract.
    pub fn register_fixed_rule_arc(&self, name: String, rule: Arc<dyn FixedRule>) -> Result<()> {
        let mut registry = self
            .fixed_rules
            .write()
            .expect("fixed-rule registry poisoned");
        if registry.contains_key(&name) {
            bail!(FixedRuleNameConflict(name));
        }
        registry.insert(name, rule);
        Ok(())
    }

    /// Unregister a custom fixed rule. Returns whether it existed. Built-ins
    /// cannot be removed (they are never reported as removed).
    pub fn unregister_fixed_rule(&self, name: &str) -> bool {
        if DEFAULT_FIXED_RULES.contains_key(name) {
            return false;
        }
        self.fixed_rules
            .write()
            .expect("fixed-rule registry poisoned")
            .remove(name)
            .is_some()
    }

    /// The next callback registration id (monotonic per `Db`).
    pub(crate) fn next_callback_id(&self) -> u32 {
        self.callback_count.fetch_add(1, Ordering::SeqCst) + 1
    }

    // ─────────────────────────────────────────────────────────────────────
    // Script entry
    // ─────────────────────────────────────────────────────────────────────

    /// Parse and run a script with default evaluation options.
    pub fn run_script(
        &self,
        payload: &str,
        params: BTreeMap<String, DataValue>,
    ) -> Result<NamedRows> {
        self.run_script_with(payload, params, ScriptOptions::default())
    }

    /// Parse and run a script under explicit evaluation options (budget
    /// ceilings, deadline).
    pub fn run_script_with(
        &self,
        payload: &str,
        params: BTreeMap<String, DataValue>,
        options: ScriptOptions,
    ) -> Result<NamedRows> {
        let cur_vld = current_validity()?;
        let fixed = self.fixed_rules();
        match parse_script(payload, &params, &fixed, cur_vld)? {
            Script::Single(prog) => self.execute_single(*prog, cur_vld, &options),
            Script::Sys(op) => self.run_sys_op(op, cur_vld, &options),
            Script::Imperative(_) => bail!(ImperativeNotWired),
        }
    }

    /// Execute one query or mutation. A mutation (a query with a
    /// `store_relation` output) opens a write session and commits with
    /// conflict retry; a pure query opens a read session.
    fn execute_single(
        &self,
        program: InputProgram,
        cur_vld: ValidityTs,
        options: &ScriptOptions,
    ) -> Result<NamedRows> {
        // Temp-relation store ops (`:create _t {…}` etc.) are refused, not
        // routed: `needs_write_lock` deliberately excludes temporaries, and
        // the read path ignores `store_relation`, so without this check the
        // mutation would be SILENTLY dropped (hostile-review finding F2).
        // The refusal stands until multi-script sessions land — in this
        // tier the session's temp store dies with the script, so a temp
        // write could never be observed by any later query.
        // NOTE(constraints-builder): `#[allow]` reconciles a clippy
        // toolchain-version drift (collapsible_if / let-chains) in this
        // session-tier block; the block itself is the session author's F2
        // fix, not constraint work.
        #[allow(clippy::collapsible_if)]
        if let Some((h, _, _, _)) = &program.out_opts().store_relation
            && h.name.is_temp_relation_name()
        {
            bail!(TempRelationNotReachableError(
                h.name.name.to_string(),
                h.span
            ));
        }
        if program.needs_write_lock().is_some() {
            let callback_targets = self.current_callback_targets();
            crate::storage::retry::retry_on_conflict_with_backoff(MAX_COMMIT_ATTEMPTS, || {
                // Fresh transaction AND fresh collector per attempt: a
                // conflicted attempt is discarded whole, so no phantom events.
                let mut collector = CallbackCollector::default();
                let mut tx = SessionTx::new_write(self.storage.write_tx()?, options.clone());
                let rows = self.run_query(
                    &mut tx,
                    program.clone(),
                    cur_vld,
                    &callback_targets,
                    &mut collector,
                    0,
                )?;
                // Integrity constraints: the denial check. Every constraint
                // of every relation this transaction mutated (user writes
                // and trigger writes alike) is evaluated against the
                // post-write state; a non-empty result is a typed refusal
                // and the whole transaction rolls back.
                self.enforce_constraints(&mut tx, cur_vld)?;
                // Segment soundness: bumps precede the commit, so any
                // snapshot that can see these writes sees the new marks.
                for rel in &tx.touched_relations {
                    self.segments.bump_before_commit(*rel);
                }
                tx.store.commit()?;
                // Post-commit only: retirements are durable, so their
                // segments and watermarks leave the engine now (a
                // rolled-back destroy never reaches this line).
                for rel in &tx.retired_relations {
                    self.segments.evict(*rel);
                }
                // The universe is durable, now tell observers.
                self.send_callbacks(collector);
                Ok(rows)
            })
        } else {
            let mut tx = SessionTx::new_read(self.storage.read_tx()?, options.clone());
            self.run_query_readonly(&mut tx, program, cur_vld)
        }
    }

    // ─────────────────────────────────────────────────────────────────────
    // The query pipeline
    // ─────────────────────────────────────────────────────────────────────

    /// Compile a program against the session's read surface and evaluate it
    /// semi-naively, returning the raw result store, the entry head, and the
    /// output options. This is the read-only heart shared by every path
    /// (including constraint enforcement, `runtime/constraint.rs`).
    pub(crate) fn compile_and_eval<T: ReadTx>(
        &self,
        store: &T,
        temp: &TempTx,
        program: InputProgram,
        cur_vld: ValidityTs,
        options: &ScriptOptions,
        segments: crate::engines::segments::Segments<'_>,
    ) -> Result<(EpochStore, bool, Vec<Symbol>, QueryOutOptions)> {
        let view = SessionView { store, temp };
        let out_opts = program.out_opts().clone();
        let head = program.get_entry_out_head_or_default()?;

        // One kill flag shared by the budget (checked at epoch barriers),
        // every fixed rule's `CancelFlag` (checked inside long algorithms),
        // and every search atom (checked once per search invocation), so a
        // cancelled or deadline-exceeded query stops them all.
        let kill = Arc::new(AtomicBool::new(false));
        let cancel = CancelFlag(kill.clone());

        let mut normalizer = SessionNormalizer::new(view, cancel.clone());
        let (nf, _) = program.into_normalized_program(&mut normalizer)?;
        let (strat, lifetimes) = nf.into_stratified_program()?;
        let magic = strat.magic_sets_rewrite(&view)?;
        let compiled = stratified_magic_compile(store, magic)?;
        // ONE machine: vectorized execution end to end, judged by the
        // naive oracle. (The row-at-a-time twin was deleted; criterion on
        // a loaded 32-core box had it losing or tying everywhere it was
        // measured against the batch pipeline.)
        let eval_prog =
            crate::query::compile::bind_for_eval(&compiled, store, segments, &mut |app| {
                Ok(SessionFixedRule::new(app, view, cancel.clone()))
            })?;

        // Eval applies take/skip only when the query is not sorted; a sorted
        // query must see every row before ordering (upstream's rule).
        let limit = if out_opts.sorters.is_empty() {
            RowLimit {
                num_to_take: out_opts.num_to_take(),
                num_to_skip: out_opts.offset,
            }
        } else {
            RowLimit::default()
        };

        let _ = cur_vld;
        let budget = build_budget(options, &out_opts, kill)?;
        let outcome = stratified_evaluate(&eval_prog, &lifetimes, limit, &budget, None)?;
        Ok((outcome.store, outcome.limited, head, out_opts))
    }

    /// Turn an evaluated result store into the final row vector: apply the
    /// `:order` sort and the limit/offset, then check any `:assert`.
    fn finalize_rows(
        result: &EpochStore,
        limited: bool,
        head: &[Symbol],
        out_opts: &QueryOutOptions,
    ) -> Result<Vec<Tuple>> {
        let rows: Vec<Tuple> = if out_opts.sorters.is_empty() {
            // Eval already applied take/skip. The two iterators are distinct
            // opaque types, so each branch collects on its own.
            if limited {
                result
                    .early_returned_iter()
                    .map(|t| t.into_tuple())
                    .collect()
            } else {
                result.all_iter().map(|t| t.into_tuple()).collect()
            }
        } else {
            let sorted = sort_and_collect(result, &out_opts.sorters, head)?;
            let skip = out_opts.offset.unwrap_or(0);
            let sorted = sorted.into_iter().skip(skip);
            match out_opts.limit {
                Some(n) => sorted.take(n).collect(),
                None => sorted.collect(),
            }
        };

        if let Some(assertion) = &out_opts.assertion {
            match assertion {
                QueryAssertion::AssertNone(span) => {
                    if let Some(first) = rows.first() {
                        bail!(QueryAssertionFailure(
                            format!(
                                "the query is required to return no rows, but it returned {first:?}"
                            ),
                            *span,
                        ));
                    }
                }
                QueryAssertion::AssertSome(span) => {
                    if rows.is_empty() {
                        bail!(QueryAssertionFailure(
                            "the query is required to return some rows, but it returned none"
                                .to_string(),
                            *span,
                        ));
                    }
                }
            }
        }
        Ok(rows)
    }

    /// Run a query or mutation to completion inside a write session. Used
    /// for top-level mutations (`trigger_depth` 0) and for trigger
    /// recursion, which passes the parent's depth + 1; the mutation
    /// pipeline refuses a cascade past its typed ceiling
    /// ([`crate::runtime::mutate::MAX_TRIGGER_CASCADE_DEPTH`]).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn run_query(
        &self,
        tx: &mut SessionTx<S::WriteTx>,
        program: InputProgram,
        cur_vld: ValidityTs,
        callback_targets: &BTreeSet<SmartString<LazyCompact>>,
        callback_collector: &mut CallbackCollector,
        trigger_depth: usize,
    ) -> Result<NamedRows> {
        let options = tx.options.clone();
        // Pre-mutation preconditions on the output relation.
        if let Some((meta, op, _, _)) = &program.out_opts().store_relation {
            let exists = tx.get_relation(&meta.name.name).is_ok();
            match op {
                RelationOp::Create if exists => {
                    bail!(StoreRelationPrecondition(format!(
                        "cannot :create relation '{}': it already exists",
                        meta.name.name
                    )));
                }
                RelationOp::Create | RelationOp::Replace => {}
                _ if !exists => {
                    bail!(StoreRelationPrecondition(format!(
                        "relation '{}' does not exist",
                        meta.name.name
                    )));
                }
                _ => {}
            }
        }

        // Segments are COMMITTED-state mirrors; a write session's queries
        // (including trigger and constraint evaluation) read the tx's own
        // uncommitted view, so the segment context is OFF here — typed
        // dirty-read protection, pinned by the constraint suite.
        let (result, limited, head, out_opts) = self.compile_and_eval(
            &tx.store,
            &tx.temp,
            program,
            cur_vld,
            &options,
            crate::engines::segments::Segments::OFF,
        )?;
        let rows = Self::finalize_rows(&result, limited, &head, &out_opts)?;

        match out_opts.store_relation {
            Some((meta, op, ret, write_vld)) => {
                let force_collect = if ret == ReturnMutation::Returning {
                    meta.name.name.as_str()
                } else {
                    ""
                };
                tx.execute_relation(
                    self,
                    rows.into_iter(),
                    op,
                    &meta,
                    &head,
                    cur_vld,
                    write_vld,
                    callback_targets,
                    callback_collector,
                    trigger_depth,
                    force_collect,
                )?;
                // A mutation reports a small status unless `:returning` asked
                // for the rows, which the mutation pipeline routed through the
                // collector.
                if ret == ReturnMutation::Returning {
                    Ok(returning_rows(callback_collector, &meta.name.name))
                } else {
                    Ok(NamedRows::new(
                        vec!["status".to_string()],
                        vec![vec![DataValue::from("OK")]],
                    ))
                }
            }
            None => Ok(materialize(rows, &head)),
        }
    }

    /// Run a pure query inside a read session. A read session cannot mutate,
    /// so a `store_relation` output (which `needs_write_lock` would have
    /// routed to the write path) is a caller error here.
    fn run_query_readonly<T: ReadTx>(
        &self,
        tx: &mut SessionTx<T>,
        program: InputProgram,
        cur_vld: ValidityTs,
    ) -> Result<NamedRows> {
        let options = tx.options.clone();
        let (result, limited, head, out_opts) = self.compile_and_eval(
            &tx.store,
            &tx.temp,
            program,
            cur_vld,
            &options,
            crate::engines::segments::Segments(Some(&self.segments)),
        )?;
        let rows = Self::finalize_rows(&result, limited, &head, &out_opts)?;
        Ok(materialize(rows, &head))
    }

    // ─────────────────────────────────────────────────────────────────────
    // System ops
    // ─────────────────────────────────────────────────────────────────────

    fn run_sys_op(
        &self,
        op: SysOp,
        cur_vld: ValidityTs,
        options: &ScriptOptions,
    ) -> Result<NamedRows> {
        match op {
            // Integrity constraints (runtime/constraint.rs). Creation
            // evaluates the body over the full current state under the
            // caller's budget.
            SysOp::CreateConstraint(name, source) => {
                self.sys_create_constraint(&name, &source, cur_vld, options)
            }
            SysOp::RemoveConstraint(name) => self.sys_remove_constraint(&name),
            SysOp::ListConstraints => self.sys_list_constraints(),
            // Read-only catalog ops.
            SysOp::ListRelations => {
                let tx = SessionTx::new_read(self.storage.read_tx()?, ScriptOptions::default());
                let mut rows = vec![];
                for handle in list_relations(&tx.store)? {
                    rows.push(vec![
                        DataValue::from(handle.name.as_str()),
                        DataValue::from(handle.arity() as i64),
                        DataValue::from(format!("{:?}", handle.access_level)),
                    ]);
                }
                Ok(NamedRows::new(
                    vec!["name".into(), "arity".into(), "access_level".into()],
                    rows,
                ))
            }
            SysOp::ListColumns(name) => {
                let tx = SessionTx::new_read(self.storage.read_tx()?, ScriptOptions::default());
                let handle = get_relation(&tx.store, &name.name)?;
                let mut rows = vec![];
                for col in handle
                    .metadata
                    .keys
                    .iter()
                    .map(|c| (c, true))
                    .chain(handle.metadata.non_keys.iter().map(|c| (c, false)))
                {
                    rows.push(vec![
                        DataValue::from(col.0.name.as_str()),
                        DataValue::from(col.1),
                    ]);
                }
                Ok(NamedRows::new(vec!["column".into(), "is_key".into()], rows))
            }
            SysOp::ListFixedRules => {
                let rows = self
                    .fixed_rules()
                    .keys()
                    .map(|k| vec![DataValue::from(k.as_str())])
                    .collect();
                Ok(NamedRows::new(vec!["name".into()], rows))
            }
            SysOp::ShowTrigger(name) => {
                let tx = SessionTx::new_read(self.storage.read_tx()?, ScriptOptions::default());
                let handle = get_relation(&tx.store, &name.name)?;
                let mut rows = vec![];
                for (kind, src) in handle
                    .put_triggers
                    .iter()
                    .map(|s| ("on_put", s))
                    .chain(handle.rm_triggers.iter().map(|s| ("on_rm", s)))
                    .chain(handle.replace_triggers.iter().map(|s| ("on_replace", s)))
                {
                    rows.push(vec![DataValue::from(kind), DataValue::from(src.as_str())]);
                }
                Ok(NamedRows::new(vec!["kind".into(), "source".into()], rows))
            }
            SysOp::ListRunning => Ok(NamedRows::new(
                vec!["id".into(), "started_at".into()],
                vec![],
            )),

            // Write catalog ops (retry on conflict).
            SysOp::RemoveRelation(names) => self.sys_write(|tx| {
                for name in &names {
                    tx.destroy_relation(&name.name)?;
                }
                Ok(status_ok())
            }),
            SysOp::RenameRelation(pairs) => self.sys_write(|tx| {
                for (old, new) in &pairs {
                    rename_relation(&mut tx.store, old, new)?;
                }
                Ok(status_ok())
            }),
            SysOp::DescribeRelation(name, desc) => self.sys_write(|tx| {
                describe_relation(&mut tx.store, &name.name, &desc)?;
                Ok(status_ok())
            }),
            SysOp::SetTriggers(name, puts, rms, replaces) => self.sys_write(move |tx| {
                set_relation_triggers(&mut tx.store, &name, &puts, &rms, &replaces)?;
                Ok(status_ok())
            }),
            SysOp::SetAccessLevel(names, level) => {
                let level = map_access_level(level);
                self.sys_write(move |tx| {
                    for name in &names {
                        set_access_level(&mut tx.store, &name.name, level)?;
                    }
                    Ok(status_ok())
                })
            }
            SysOp::Compact => {
                self.storage.sync()?;
                Ok(status_ok())
            }
            SysOp::MerkleRoot(rel) => {
                // A cold root is a full ordered rescan, so the scan must be
                // bounded: the session's derived-tuple ceiling doubles as the
                // scan ceiling (one scanned pair = one unit), with a default
                // when the caller sets none. A ceiling of zero refuses before
                // scanning anything.
                let ceiling = match options.derived_tuple_ceiling {
                    Some(c) => NonZeroU64::new(c)
                        .ok_or(crate::storage::merkle::MerkleScanExceeded { ceiling: 0 })?,
                    None => DEFAULT_MERKLE_SCAN_CEILING,
                };
                let rtx = self.storage.read_tx()?;
                let root = match rel {
                    None => crate::storage::merkle::state_root(&rtx, ceiling)?,
                    Some(name) => {
                        let id = get_relation(&rtx, &name.name)?.id;
                        crate::storage::merkle::relation_root(&rtx, id, ceiling)?
                    }
                };
                Ok(NamedRows::new(
                    vec!["root".into()],
                    vec![vec![DataValue::from(root.to_hex())]],
                ))
            }

            // Not yet: explain, kill, and every index-operator op.
            SysOp::Explain(_) => bail!(IndexOpNotLanded("::explain")),
            SysOp::KillRunning(_) => bail!(IndexOpNotLanded("::kill")),
            SysOp::ListIndices(name) => {
                let _ = cur_vld;
                let tx = SessionTx::new_read(self.storage.read_tx()?, ScriptOptions::default());
                let handle = get_relation(&tx.store, &name.name)?;
                let rows = handle
                    .indices
                    .iter()
                    .map(|r| {
                        let kind = match &r.kind {
                            crate::runtime::relation::IndexKind::Plain { .. } => "plain",
                            crate::runtime::relation::IndexKind::Hnsw(..) => "hnsw",
                            crate::runtime::relation::IndexKind::Fts(..) => "fts",
                            crate::runtime::relation::IndexKind::Lsh { .. } => "lsh",
                        };
                        vec![DataValue::from(r.name.as_str()), DataValue::from(kind)]
                    })
                    .collect();
                Ok(NamedRows::new(vec!["name".into(), "kind".into()], rows))
            }
            SysOp::CreateIndex(rel, name, cols) => {
                self.sys_write(|tx| tx.create_plain_index(&rel.name, &name.name, &cols))
            }
            SysOp::CreateVectorIndex(cfg) => self.sys_write(|tx| tx.create_hnsw_index(&cfg)),
            SysOp::CreateFtsIndex(cfg) => self.sys_write(|tx| tx.create_fts_index(&cfg)),
            SysOp::CreateMinHashLshIndex(cfg) => self.sys_write(|tx| tx.create_lsh_index(&cfg)),
            SysOp::RemoveIndex(rel, idx) => {
                self.sys_write(|tx| tx.remove_index(&rel.name, &idx.name))
            }
        }
    }

    /// Run a catalog-mutating closure inside a write session, committing with
    /// conflict retry.
    fn sys_write(
        &self,
        f: impl Fn(&mut SessionTx<S::WriteTx>) -> Result<NamedRows>,
    ) -> Result<NamedRows> {
        crate::storage::retry::retry_on_conflict_with_backoff(MAX_COMMIT_ATTEMPTS, || {
            let mut tx = SessionTx::new_write(self.storage.write_tx()?, ScriptOptions::default());
            let out = f(&mut tx)?;
            for rel in &tx.touched_relations {
                self.segments.bump_before_commit(*rel);
            }
            tx.store.commit()?;
            for rel in &tx.retired_relations {
                self.segments.evict(*rel);
            }
            Ok(out)
        })
    }
}

/// Build the evaluation budget from the caller's options and the query's own
/// `:timeout`. The epoch ceiling is deterministic (checked at epoch
/// barriers); the derived-tuple ceiling is deterministic; only the deadline
/// is wall-clock.
fn build_budget(
    options: &ScriptOptions,
    out_opts: &QueryOutOptions,
    kill: Arc<AtomicBool>,
) -> Result<Budget> {
    let ceiling = options.epoch_ceiling.unwrap_or(DEFAULT_EPOCH_CEILING);
    let ceiling = NonZeroU32::new(ceiling.max(1)).expect("max(1) is nonzero");
    let mut budget = Budget::new(ceiling).with_kill_flag(kill);
    if let Some(n) = options.derived_tuple_ceiling {
        budget = budget.with_derived_tuple_ceiling(n);
    }
    // The tighter of the caller's deadline and the query's own :timeout.
    let deadline = [options.timeout_secs, out_opts.timeout]
        .into_iter()
        .flatten()
        .filter(|s| *s > 0.0)
        .min_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    if let Some(secs) = deadline {
        let duration = Duration::try_from_secs_f64(secs).map_err(|_| InvalidTimeout(secs))?;
        budget = budget.with_timeout(duration);
    }
    Ok(budget)
}

/// Materialize a row vector into a headered result.
fn materialize(rows: Vec<Tuple>, head: &[Symbol]) -> NamedRows {
    let headers = head.iter().map(|s| s.name.to_string()).collect();
    NamedRows::new(headers, rows)
}

/// A one-cell `status: OK` result for ops that report success, not rows.
pub(crate) fn status_ok() -> NamedRows {
    NamedRows::new(
        vec!["status".to_string()],
        vec![vec![DataValue::from("OK")]],
    )
}

/// Build the `:returning` result from what the mutation collected. The
/// collector holds `(op, new, old)` events; `:returning` reports the new rows.
fn returning_rows(collector: &CallbackCollector, relation: &str) -> NamedRows {
    let mut headers = vec![];
    let mut rows = vec![];
    if let Some(events) = collector.get(relation) {
        for (_op, new, _old) in events {
            if headers.is_empty() {
                headers = new.headers.clone();
            }
            rows.extend(new.rows.iter().cloned());
        }
    }
    NamedRows::new(headers, rows)
}

/// Map the parser's access-level enum to the catalog's. Both are the same
/// four-rung ladder; the parse tier and runtime tier keep distinct types.
fn map_access_level(level: ParseAccessLevel) -> AccessLevel {
    match level {
        ParseAccessLevel::Hidden => AccessLevel::Hidden,
        ParseAccessLevel::ReadOnly => AccessLevel::ReadOnly,
        ParseAccessLevel::Protected => AccessLevel::Protected,
        ParseAccessLevel::Normal => AccessLevel::Normal,
    }
}

// ─────────────────────────────────────────────────────────────────────────
// The session transaction
// ─────────────────────────────────────────────────────────────────────────

/// One session: a backend transaction `T` plus a private scratch store for
/// temp (`_`-prefixed) relations and a per-session trigger-parse cache.
///
/// The species law: catalog reads and scans need only [`ReadTx`]; every
/// mutation method requires `T: WriteTx`, so a mutation on a read session is
/// a compile error. The session owns its transaction — no borrowed `'s`
/// lifetime is threaded through the engine — and is `Send`.
pub struct SessionTx<T> {
    /// Resolved manifest-index contexts (compiled extractors, analyzers,
    /// permutations), cached per index relation name for this session.
    pub(crate) index_ctxs: std::collections::BTreeMap<
        smartstring::SmartString<smartstring::LazyCompact>,
        crate::runtime::mutate::IndexCtx,
    >,
    pub(crate) store: T,
    pub(crate) temp: TempTx,
    /// Trigger source → parsed program, parsed once per session. Sound
    /// because a session has one `cur_vld`, which parsing substitutes.
    /// Constraint bodies share this cache (same convention: raw source in
    /// the catalog, parsed once per session).
    pub(crate) parsed_triggers: BTreeMap<SmartString<LazyCompact>, InputProgram>,
    /// The evaluation controls for every query in this session, including
    /// triggers (which run under the parent's budget).
    pub(crate) options: ScriptOptions,
    /// Integrity constraints of every relation this transaction has
    /// mutated, `name → body source`, deduped by name (each relation in a
    /// constraint's read-set mirrors the identical spec). Collected by the
    /// mutation pipeline and drained by
    /// [`Db::enforce_constraints`](crate::runtime::db::Db) before commit.
    pub(crate) pending_constraints: BTreeMap<SmartString<LazyCompact>, String>,
    /// Every relation id this transaction wrote (user writes, trigger
    /// writes, index backfills alike) — drained into segment-watermark
    /// bumps BEFORE the storage commit (the segments' soundness rule).
    pub(crate) touched_relations: std::collections::BTreeSet<crate::data::tuple::RelationId>,
    /// Relation ids permanently retired by this transaction (destroy /
    /// replace / index drop) — drained into segment-engine evictions
    /// AFTER a successful commit (a rolled-back destroy retires nothing).
    pub(crate) retired_relations: std::collections::BTreeSet<crate::data::tuple::RelationId>,
}

impl<T: ReadTx> SessionTx<T> {
    pub(crate) fn new_read(store: T, options: ScriptOptions) -> Self {
        Self {
            store,
            temp: TempTx::default(),
            parsed_triggers: BTreeMap::new(),
            index_ctxs: BTreeMap::new(),
            options,
            pending_constraints: BTreeMap::new(),
            touched_relations: std::collections::BTreeSet::new(),
            retired_relations: std::collections::BTreeSet::new(),
        }
    }

    /// Catalog lookup, routed by name: `_`-prefixed names live in the
    /// session's temp catalog, everything else in the persistent catalog.
    pub(crate) fn get_relation(&self, name: &str) -> Result<RelationHandle> {
        if name.starts_with('_') {
            get_relation(&self.temp, name)
        } else {
            get_relation(&self.store, name)
        }
    }

    /// Read one key from the store the relation lives in.
    pub(crate) fn get_routed(&self, is_temp: bool, key: &[u8]) -> Result<Option<Vec<u8>>> {
        if is_temp {
            self.temp.get(key)
        } else {
            self.store.get(key)
        }
    }

    /// Existence of one key in the store the relation lives in.
    pub(crate) fn exists_routed(&self, is_temp: bool, key: &[u8]) -> Result<bool> {
        if is_temp {
            self.temp.exists(key)
        } else {
            self.store.exists(key)
        }
    }

    /// The fact's LOGICAL row governing AT `valid`, routed: the versioned
    /// format's point read (a bitemporal probe under the fact's key
    /// prefix, resolved with the newest system knowledge), replacing
    /// exact-key reads for relation rows.
    ///
    /// `valid` is the write's OWN target instant — "the row this write
    /// supersedes" must mean "whatever governed the instant being
    /// written", not some unrelated later instant a different write
    /// happened to land at. The three write paths in `runtime/mutate.rs`
    /// pass their own resolved `WriteValidity` coordinate; `:ensure` /
    /// `:ensure_not` (which can never carry a `@` clause) pass
    /// [`crate::data::value::MAX_VALIDITY_TS`] for the ordinary "does this
    /// exist at all, right now" question. For an unspecified-`@` write
    /// `valid` is the transaction's own system stamp, which is always at
    /// or past every instant an ordinary (non-`@`) history could contain,
    /// so this is byte-for-byte the old "newest ever" behavior whenever no
    /// write anywhere has used `@` — only an explicit historical or
    /// future-dated `@` write can make the two diverge.
    ///
    /// Under SSI in a write transaction the probe conflict-tracks its
    /// range — the WHOLE fact-key prefix, independent of `valid` — so
    /// uniqueness races on the fact abort one racer regardless of which
    /// instant either racer targets.
    pub(crate) fn current_row_routed(
        &self,
        handle: &RelationHandle,
        key_cols: &[DataValue],
        valid: ValidityTs,
        span: SourceSpan,
    ) -> Result<Option<Tuple>> {
        let as_of = AsOf::current(valid);
        if handle.is_temp {
            handle.current_row(&self.temp, key_cols, as_of, span)
        } else {
            handle.current_row(&self.store, key_cols, as_of, span)
        }
    }
}

impl<T: WriteTx> SessionTx<T> {
    pub(crate) fn new_write(store: T, options: ScriptOptions) -> Self {
        Self {
            store,
            temp: TempTx::default(),
            parsed_triggers: BTreeMap::new(),
            index_ctxs: BTreeMap::new(),
            options,
            pending_constraints: BTreeMap::new(),
            touched_relations: std::collections::BTreeSet::new(),
            retired_relations: std::collections::BTreeSet::new(),
        }
    }

    /// The system stamp every bitemporal row written to the given store
    /// carries, routed like the row itself: the persistent store's stamp
    /// comes from the storage's monotone clock, the session temp store's
    /// from its logical clock. One stamp per transaction per store — a
    /// transaction's writes are one instant of recorded history.
    pub(crate) fn system_stamp_routed(&self, is_temp: bool) -> ValidityTs {
        if is_temp {
            self.temp.system_stamp()
        } else {
            self.store.system_stamp()
        }
    }

    /// Register a mutated relation's integrity constraints for the
    /// pre-commit denial check. Idempotent per constraint name: the same
    /// constraint mirrored on several touched relations is checked once.
    pub(crate) fn note_constraints(&mut self, handle: &RelationHandle) {
        for c in &handle.constraints {
            self.pending_constraints
                .entry(c.name.clone())
                .or_insert_with(|| c.source.clone());
        }
    }

    /// Create a relation, routed to the temp or persistent catalog by name.
    pub(crate) fn create_relation(
        &mut self,
        input: crate::data::program::InputRelationHandle,
        keyspace_kind: KeyspaceKind,
    ) -> Result<RelationHandle> {
        if input.name.name.starts_with('_') {
            create_relation(&mut self.temp, input, keyspace_kind)
        } else {
            create_relation(&mut self.store, input, keyspace_kind)
        }
    }

    /// Destroy a relation (catalog row and keyspace, in-transaction), routed
    /// by name. The retired id is recorded so the session evicts its
    /// segment and watermark after commit — every permanent retirement
    /// (remove, replace, ::index drop, LSH inverse drop) funnels through
    /// here, so none can leak (hostile-review finding: three sibling
    /// destroy sites leaked one engine entry per cycle, forever).
    pub(crate) fn destroy_relation(&mut self, name: &str) -> Result<()> {
        if name.starts_with('_') {
            destroy_relation(&mut self.temp, name)
        } else {
            if let Ok(handle) = self.get_relation(name) {
                self.retired_relations.insert(handle.id);
            }
            destroy_relation(&mut self.store, name)
        }
    }

    /// Re-write a relation's catalog row (e.g. to re-attach triggers across a
    /// `:replace`), routed by the handle's `is_temp`.
    pub(crate) fn write_relation_row(&mut self, handle: &RelationHandle) -> Result<()> {
        if handle.is_temp {
            write_relation_row(&mut self.temp, handle)
        } else {
            write_relation_row(&mut self.store, handle)
        }
    }

    /// Write one key/value into the store the relation lives in.
    pub(crate) fn put_routed(&mut self, is_temp: bool, key: &[u8], val: &[u8]) -> Result<()> {
        if is_temp {
            self.temp.put(key, val)
        } else {
            self.store.put(key, val)
        }
    }

    /// Delete one key from the store the relation lives in.
    pub(crate) fn del_routed(&mut self, is_temp: bool, key: &[u8]) -> Result<()> {
        if is_temp {
            self.temp.del(key)
        } else {
            self.store.del(key)
        }
    }

    /// Parse a trigger's source once per session and cache the program. The
    /// session's single `cur_vld` is baked in at first parse; every later
    /// firing clones the cached program.
    pub(crate) fn parsed_trigger(
        &mut self,
        source: &str,
        fixed_rules: &BTreeMap<String, Arc<dyn FixedRule>>,
        cur_vld: ValidityTs,
    ) -> Result<InputProgram> {
        if let Some(prog) = self.parsed_triggers.get(source) {
            return Ok(prog.clone());
        }
        let prog =
            parse_script(source, &BTreeMap::new(), fixed_rules, cur_vld)?.get_single_program()?;
        self.parsed_triggers
            .insert(SmartString::from(source), prog.clone());
        Ok(prog)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::fjall::new_fjall_storage;
    use crate::storage::sim::SimStorage;

    fn no_params() -> BTreeMap<String, DataValue> {
        BTreeMap::new()
    }

    /// The segment law, end to end: a pure read builds and serves the
    /// relation's current-state segment; ANY committed write to the
    /// relation orphans it (a re-read sees the write, never the cached
    /// past); a DENIED write leaves state and answers untouched; and the
    /// same query inside a write session reads the transaction's own
    /// uncommitted view, never a committed-state segment.
    #[test]
    fn segments_serve_fresh_and_never_dirty() {
        let db = Db::new(SimStorage::new(7)).unwrap();
        db.run_script(
            "?[k, v] <- [[1, 10], [2, 20]] :create w {k => v}",
            no_params(),
        )
        .unwrap();

        // First read builds the segment; second is served from it.
        let q = "?[k, v] := *w[k, v]";
        assert_eq!(
            int_rows(&db.run_script(q, no_params()).unwrap()),
            vec![vec![1, 10], vec![2, 20]]
        );
        assert_eq!(
            int_rows(&db.run_script(q, no_params()).unwrap()),
            vec![vec![1, 10], vec![2, 20]]
        );

        // A committed write orphans the segment: the re-read sees it.
        db.run_script("?[k, v] <- [[3, 30]] :put w {k, v}", no_params())
            .unwrap();
        assert_eq!(
            int_rows(&db.run_script(q, no_params()).unwrap()),
            vec![vec![1, 10], vec![2, 20], vec![3, 30]]
        );

        // A retraction is a write like any other: served state updates.
        db.run_script("?[k, v] <- [[2, 20]] :rm w {k, v}", no_params())
            .unwrap();
        assert_eq!(
            int_rows(&db.run_script(q, no_params()).unwrap()),
            vec![vec![1, 10], vec![3, 30]]
        );
        assert_eq!(
            int_rows(&db.run_script(q, no_params()).unwrap()),
            vec![vec![1, 10], vec![3, 30]]
        );

        // A write whose transaction rolls back (parse-stage failure after
        // the relation was touched is hard to stage; a constraint denial
        // is the canonical rollback) leaves both state and served answers
        // untouched — the early bump merely orphans, never lies.
        db.run_script(
            "::constraint create nonneg { ?[k, v] := *w[k, v], v < 0 }",
            no_params(),
        )
        .unwrap();
        assert!(
            db.run_script("?[k, v] <- [[4, -1]] :put w {k, v}", no_params())
                .is_err(),
            "violating write denied"
        );
        assert_eq!(
            int_rows(&db.run_script(q, no_params()).unwrap()),
            vec![vec![1, 10], vec![3, 30]]
        );

        // A PLAIN INDEX is a mutated relation in its own right: its
        // segment must orphan when a base-relation write updates the
        // mirrored rows (hostile-review reproducer: the served index
        // segment returned the stale two-row past after a base `:put`).
        db.run_script("::index create w:by_v {v}", no_params())
            .unwrap();
        let qi = "?[v, k] := *w:by_v{v, k}";
        assert_eq!(
            int_rows(&db.run_script(qi, no_params()).unwrap()),
            vec![vec![10, 1], vec![30, 3]]
        );
        db.run_script("?[k, v] <- [[5, 50]] :put w {k, v}", no_params())
            .unwrap();
        assert_eq!(
            int_rows(&db.run_script(qi, no_params()).unwrap()),
            vec![vec![10, 1], vec![30, 3], vec![50, 5]],
            "an index segment must never outlive a base write"
        );
    }

    /// [`segments_serve_fresh_and_never_dirty`]'s reproducer, extended to
    /// the JOIN PROBE path (issue #75's fix): `*jl[k], *jr[k, v]` compiles
    /// to a prefix join whose right side (`jr`) is now served, current-
    /// state, straight from its segment (`StoredRA::prefix_join_batched`)
    /// instead of the bitemporal seek-based probe. The probe side must
    /// obey the identical freshness law as a plain scan — a write to `jr`
    /// bumps its watermark BEFORE commit, so the very next read's witness
    /// can never match a segment built before that write, and the join
    /// sees the new row immediately, never a cached probe answer.
    #[test]
    fn segments_serve_fresh_and_never_dirty_for_join_probes() {
        let db = Db::new(SimStorage::new(9)).unwrap();
        db.run_script("?[k] <- [[1], [2]] :create jl {k}", no_params())
            .unwrap();
        db.run_script("?[k2, v] <- [[1, 10]] :create jr {k2 => v}", no_params())
            .unwrap();

        // First read builds jr's segment and serves the point-lookup probe
        // from it; second read is served from the cached segment.
        let q = "?[k, v] := *jl[k], *jr[k, v]";
        assert_eq!(
            int_rows(&db.run_script(q, no_params()).unwrap()),
            vec![vec![1, 10]]
        );
        assert_eq!(
            int_rows(&db.run_script(q, no_params()).unwrap()),
            vec![vec![1, 10]]
        );

        // A committed write to jr — the PROBE side of the join — orphans
        // its segment: the re-read must see the new row, not a stale
        // probe answer served from before the write.
        db.run_script("?[k2, v] <- [[2, 20]] :put jr {k2 => v}", no_params())
            .unwrap();
        assert_eq!(
            int_rows(&db.run_script(q, no_params()).unwrap()),
            vec![vec![1, 10], vec![2, 20]]
        );
        assert_eq!(
            int_rows(&db.run_script(q, no_params()).unwrap()),
            vec![vec![1, 10], vec![2, 20]]
        );

        // A retraction on the probe side is a write like any other.
        db.run_script("?[k2, v] <- [[1, 10]] :rm jr {k2, v}", no_params())
            .unwrap();
        assert_eq!(
            int_rows(&db.run_script(q, no_params()).unwrap()),
            vec![vec![2, 20]]
        );
    }

    /// Result rows as sorted `i64` vectors, for order-independent assertions.
    fn int_rows(nr: &NamedRows) -> Vec<Vec<i64>> {
        let mut out: Vec<Vec<i64>> = nr
            .rows
            .iter()
            .map(|r| r.iter().map(|v| v.get_int().expect("int")).collect())
            .collect();
        out.sort();
        out
    }

    /// The fixed-rule mid-run spend guard, end to end: a row-amplifying
    /// algorithm (all-pairs shortest path on a 60-node path: 3600+ rows)
    /// under a 100-row ceiling refuses typed mid-run instead of
    /// materializing the whole output.
    #[test]
    fn fixed_rule_output_respects_derived_tuple_ceiling() {
        let db = Db::new(SimStorage::new(7)).unwrap();
        let mut edges = String::from("?[a, b, w] <- [");
        for i in 0..60 {
            edges.push_str(&format!("[{}, {}, 1.0],", i, i + 1));
        }
        edges.push_str("] :create edge {a, b => w}");
        db.run_script(&edges, no_params()).expect("create edges");

        let opts = ScriptOptions {
            derived_tuple_ceiling: Some(100),
            ..Default::default()
        };
        let err = db
            .run_script_with(
                "r[a] := *edge[a, _b, _w] ?[a, b, d, p] <~ ShortestPathDijkstra(*edge[], r[])",
                no_params(),
                opts,
            )
            .expect_err("amplified output must refuse under the ceiling");
        let msg = err.to_string();
        assert!(
            msg.contains("ceiling") || msg.contains("exceeded") || msg.contains("limit"),
            "typed spend refusal, got: {msg}"
        );
    }

    /// Pins the ONE production line the unit-level eval tests can't reach:
    /// `SessionFixedRule::run`'s forwarding of the true `baseline` into
    /// `FixedRuleOutput::new_budgeted` (`query/normalize.rs`). An Ok/Err-only
    /// test can never catch a regression there — the mid-run guard's
    /// non-perturbation theorem (see `InterruptTicker`'s doc) guarantees it
    /// only ever refuses inputs the epoch barrier (fed by the real,
    /// unaffected global total) would ALSO refuse — so this downcasts to the
    /// typed refusal and pins the exact dimension AND spend.
    ///
    /// A chain of `N=10` directed edges (`i -> i+1`, sourced from a 10-edge
    /// path) gives two independently useful, empirically confirmed counts
    /// (`ShortestPathDijkstra`'s row count is exactly `N*(N+1)`, verified by
    /// a throwaway probe run before this test was written):
    /// - `r[a] := *edge[a, _b, _w]` admits exactly `B = 10` rows (the
    ///   distinct sources) in the stratum BEFORE the fixed rule's — this is
    ///   the baseline the fixed rule's stratum must see.
    /// - `ShortestPathDijkstra` itself puts exactly `F = 110` rows: enough
    ///   to cross ONE `OUTPUT_STRIDE` (64) mid-run check (at put #64, having
    ///   stored 63 rows), but under 128, so no SECOND check ever happens
    ///   inside the rule's own run.
    ///
    /// With `ceiling = 70`:
    /// - Correct baseline: the one mid-run check sees `spent = 10 + 63 =
    ///   73 > 70` — refuses immediately, typed `InFlightDerivations`,
    ///   `spent == 73`, having stored only 63 of the eventual 110 rows.
    /// - A baseline wrongly forwarded as 0: that same check sees
    ///   `spent = 0 + 63 = 63 ≤ 70` and does NOT trip; no second check
    ///   exists (F < 128), so the rule completes, all 110 rows merge, and
    ///   only THEN does the epoch barrier refuse — typed `DerivedTuples`,
    ///   `spent = 10 + 110 = 120` — a different dimension AND a different
    ///   spend. Both fields are asserted exactly so either failure mode is
    ///   caught.
    #[test]
    fn fixed_rule_dispatch_forwards_true_baseline_not_zero() {
        let db = Db::new(SimStorage::new(7)).unwrap();
        let mut edges = String::from("?[a, b, w] <- [");
        for i in 0..10 {
            edges.push_str(&format!("[{}, {}, 1.0],", i, i + 1));
        }
        edges.push_str("] :create edge {a, b => w}");
        db.run_script(&edges, no_params()).expect("create edges");

        let opts = ScriptOptions {
            derived_tuple_ceiling: Some(70),
            ..Default::default()
        };
        let err = db
            .run_script_with(
                "r[a] := *edge[a, _b, _w] ?[a, b, d, p] <~ ShortestPathDijkstra(*edge[], r[])",
                no_params(),
                opts,
            )
            .expect_err(
                "the fixed rule's mid-run guard must refuse, counting the r-stratum's baseline",
            );
        let refusal: &crate::query::eval::LimitExceeded =
            err.downcast_ref().expect("typed budget refusal");
        assert_eq!(
            refusal.dimension,
            crate::query::eval::BudgetDimension::InFlightDerivations,
            "must refuse INSIDE the fixed rule's own mid-run guard, not the later epoch \
             barrier — a `DerivedTuples` refusal here means the guard never tripped, i.e. \
             the forwarded baseline was too small (e.g. zeroed)"
        );
        assert_eq!(
            refusal.spent, 73,
            "spend must be the r-stratum's baseline(10) + this rule's own 63 rows put so \
             far; a zeroed baseline would report 63 instead (and likely not refuse here at \
             all, deferring to a much later `DerivedTuples` barrier refusal at spend 120)"
        );
    }

    /// A `:timeout` so large it cannot become a `Duration` must be a clean
    /// query error, not a panic. The parser only bounds `:timeout` by `> 0`
    /// (`parse/query.rs`), so an absurd-but-positive value like `1e300`
    /// reaches `build_budget` unfiltered; before the fix this called
    /// `Duration::from_secs_f64(1e300)` directly, which panics.
    #[test]
    fn huge_timeout_is_a_clean_error_not_a_panic() {
        let db = Db::new(SimStorage::new(7)).unwrap();
        let err = db
            .run_script("?[a] := a in [1, 2, 3] :timeout 1e300", no_params())
            .expect_err("an unrepresentable timeout must refuse cleanly");
        assert!(
            err.downcast_ref::<InvalidTimeout>().is_some(),
            "expected a typed InvalidTimeout refusal, got: {err}"
        );
    }

    /// Same reproduction via the `ScriptOptions.timeout_secs` Rust-API path
    /// (bypasses the parser's own `:timeout` handling entirely), and also
    /// covers infinity — both must be refused, never panic.
    #[test]
    fn huge_or_infinite_timeout_via_script_options_is_a_clean_error() {
        let db = Db::new(SimStorage::new(7)).unwrap();
        for bad in [1e300_f64, f64::INFINITY] {
            let opts = ScriptOptions {
                timeout_secs: Some(bad),
                ..Default::default()
            };
            let err = db
                .run_script_with("?[a] := a in [1, 2, 3]", no_params(), opts)
                .expect_err("an unrepresentable timeout must refuse cleanly, not panic");
            assert!(
                err.downcast_ref::<InvalidTimeout>().is_some(),
                "expected a typed InvalidTimeout refusal for {bad}, got: {err}"
            );
        }
    }

    /// Regression for fuzz artifact
    /// crash-f1ef21a6c4f99a02f719c5bde2689bb158df629f: a literal `i64`
    /// product that overflows 64 bits panicked in parse-time constant
    /// folding (debug builds: "attempt to multiply with overflow") and
    /// silently wrapped to a wrong answer (release builds). Both profiles
    /// must now see the same clean typed error.
    #[test]
    fn overflowing_literal_product_is_a_clean_error_not_a_panic() {
        let db = Db::new(SimStorage::new(7)).unwrap();
        let err = db
            .run_script("?[x] := x = 2222222000*867076028303", no_params())
            .expect_err("an i64-overflowing literal product must refuse cleanly");
        // The op's `IntegerOverflow` is raised inside constant folding
        // (`Expr::eval` via `Expr::partial_eval`), which wraps every op
        // error as `EvalRaisedError` (its message, not its type, survives
        // — in the struct's own help string, not its fixed `Display`) —
        // the same wrapping every other op error gets.
        let wrapped = err
            .downcast_ref::<crate::data::expr::EvalRaisedError>()
            .unwrap_or_else(|| {
                panic!("expected an EvalRaisedError wrapping the overflow, got: {err}")
            });
        assert!(
            wrapped.1.contains("integer overflow"),
            "expected an integer-overflow message, got: {}",
            wrapped.1
        );
    }

    /// THE SEARCH PIPELINE END TO END: `::hnsw create` builds and backfills
    /// the index, the mutation hook indexes a later insert, and the
    /// `~doc:emb{…}` atom drives `hnsw_knn` through parse → resolve →
    /// compile → RA → eval, appending the distance column nearest-first.
    fn hnsw_create_insert_search<S: Storage>(db: Db<S>) {
        db.run_script(
            "?[id, v] <- [[1, vec([1.0, 0.0, 0.0, 0.0])], [2, vec([0.0, 1.0, 0.0, 0.0])]] \
             :create doc {id => v: <F32; 4>}",
            no_params(),
        )
        .expect("create+insert");
        db.run_script(
            "::hnsw create doc:emb {fields: [v], dim: 4, m: 16, ef_construction: 32, \
              distance: L2}",
            no_params(),
        )
        .expect("hnsw create");
        // Inserted AFTER the index exists: the write-path hook must index it.
        db.run_script(
            "?[id, v] <- [[3, vec([0.9, 0.1, 0.0, 0.0])]] :put doc {id => v}",
            no_params(),
        )
        .expect("post-create insert");

        let out = db
            .run_script(
                "?[dist, id] := ~doc:emb{id | query: vec([1.0, 0.0, 0.0, 0.0]), k: 3, \
                  bind_distance: dist} :sort dist",
                no_params(),
            )
            .expect("hnsw search");
        // A Datalog answer is a set; :sort puts it in distance order.
        // Nearest first by squared L2: id 1 at 0, id 3 at 0.02, id 2 at 2.
        let ids: Vec<i64> = out
            .rows
            .iter()
            .map(|r| r[1].get_int().expect("id"))
            .collect();
        assert_eq!(ids, vec![1, 3, 2], "nearest-first order");
        let d0 = out.rows[0][0].get_float().expect("dist");
        let d1 = out.rows[1][0].get_float().expect("dist");
        assert!(d0.abs() < 1e-6, "exact match at distance 0, got {d0}");
        assert!((d1 - 0.02).abs() < 1e-6, "squared L2, got {d1}");
    }

    #[test]
    fn hnsw_create_insert_search_mem() {
        hnsw_create_insert_search(Db::new(SimStorage::new(7)).unwrap());
    }

    #[test]
    fn hnsw_create_insert_search_fjall() {
        let dir = tempfile::tempdir().expect("tempdir");
        hnsw_create_insert_search(Db::new(new_fjall_storage(dir.path()).unwrap()).unwrap());
    }

    /// FTS end to end: `::fts create` + a search atom with a bound score.
    #[test]
    fn fts_create_search_mem() {
        let db = Db::new(SimStorage::new(7)).unwrap();
        db.run_script(
            "?[id, body] <- [[1, 'the quick brown fox'], [2, 'lazy dogs sleep']] \
             :create doc {id => body: String}",
            no_params(),
        )
        .expect("create+insert");
        db.run_script(
            "::fts create doc:txt {extractor: body, tokenizer: Simple}",
            no_params(),
        )
        .expect("fts create");
        let out = db
            .run_script(
                "?[id, s] := ~doc:txt{id | query: 'fox', k: 5, bind_score: s}",
                no_params(),
            )
            .expect("fts search");
        assert_eq!(out.rows.len(), 1);
        assert_eq!(out.rows[0][0].get_int(), Some(1));
        assert!(out.rows[0][1].get_float().expect("score") > 0.0);
        // The searching row must survive a doc deletion (hook coverage).
        db.run_script("?[id] <- [[1]] :rm doc {id}", no_params())
            .expect("delete");
        let out = db
            .run_script("?[id] := ~doc:txt{id | query: 'fox', k: 5}", no_params())
            .expect("fts search after delete");
        assert_eq!(out.rows.len(), 0, "deleted doc left the index");
    }

    /// A single search atom whose hit count exceeds one output batch
    /// (1,200 matching docs > BATCH_ROWS = 1,024): the search executor's
    /// cross-batch resumption must deliver every hit exactly once — the
    /// same suspension state machine the materialized join pins.
    #[test]
    fn search_hits_resume_across_output_batch_boundary() {
        let db = Db::new(SimStorage::new(7)).unwrap();
        let mut script = String::from("?[id, body] <- [");
        for i in 0..1200 {
            script.push_str(&format!("[{i}, 'common fox term {i}'],"));
        }
        script.push_str("] :create doc {id => body: String}");
        db.run_script(&script, no_params()).expect("seed");
        db.run_script(
            "::fts create doc:txt {extractor: body, tokenizer: Simple}",
            no_params(),
        )
        .expect("fts create");
        let out = db
            .run_script("?[id] := ~doc:txt{id | query: 'fox', k: 1500}", no_params())
            .expect("boundary search");
        assert_eq!(
            out.rows.len(),
            1200,
            "every hit exactly once across the boundary"
        );
    }

    /// LSH end to end: near-duplicate candidates come back; `::index drop`
    /// removes the index and the search atom then refuses typed.
    #[test]
    fn lsh_create_search_drop_mem() {
        let db = Db::new(SimStorage::new(7)).unwrap();
        db.run_script(
            "?[id, body] <- [[1, 'a b c d e f g h i j'], [2, 'a b c d e f g h i z'], [3, 'q r s t u v w x y zz']] \
             :create doc {id => body: String}",
            no_params(),
        )
        .expect("create+insert");
        db.run_script(
            "::lsh create doc:sim {extractor: body, tokenizer: Simple, n_gram: 3, \
              n_perm: 64, target_threshold: 0.5}",
            no_params(),
        )
        .expect("lsh create");
        let out = db
            .run_script(
                "?[id] := ~doc:sim{id | query: 'a b c d e f g h i j', k: 5}, id != 1",
                no_params(),
            )
            .expect("lsh search");
        let ids: Vec<i64> = out
            .rows
            .iter()
            .map(|r| r[0].get_int().expect("id"))
            .collect();
        assert!(
            ids.contains(&2),
            "near-duplicate must be a candidate: {ids:?}"
        );
        assert!(!ids.contains(&3), "far row must not band-collide: {ids:?}");

        db.run_script("::index drop doc:sim", no_params())
            .expect("index drop");
        let err = db
            .run_script(
                "?[id] := ~doc:sim{id | query: 'a b c d e f g h i j', k: 5}",
                no_params(),
            )
            .expect_err("search on a dropped index must refuse");
        assert!(
            err.to_string().contains("no index named"),
            "typed refusal, got: {err}"
        );
    }

    /// THE FIRST END-TO-END QUERY: create → insert → recursive query with a
    /// join → exact rows, all through the public `Db::run_script` over a real
    /// backend. Parameterized so the same script runs on fjall and mem.
    fn create_insert_recursive_join<S: Storage>(db: Db<S>) {
        // Create the relation and insert the edges of 1→2→3→4→2 in one script.
        db.run_script(
            "?[a, b] <- [[1, 2], [2, 3], [3, 4], [4, 2]] :create edge {a, b}",
            no_params(),
        )
        .expect("create+insert");

        // Transitive closure: a recursive rule with a join against the stored
        // relation, driven semi-naively.
        let out = db
            .run_script(
                "
                path[a, b] := *edge[a, b]
                path[a, b] := *edge[a, c], path[c, b]
                ?[a, b] := path[a, b]
                ",
                no_params(),
            )
            .expect("recursive query");

        // Reachability of 1→2→3→4→2 (cycle 2-3-4): from 1 everything but 1;
        // within the cycle every pair. (Identical to the compile-tier test,
        // now reached through parse → compile → RA → eval → results.)
        assert_eq!(
            int_rows(&out),
            vec![
                vec![1, 2],
                vec![1, 3],
                vec![1, 4],
                vec![2, 2],
                vec![2, 3],
                vec![2, 4],
                vec![3, 2],
                vec![3, 3],
                vec![3, 4],
                vec![4, 2],
                vec![4, 3],
                vec![4, 4],
            ]
        );
    }

    #[test]
    fn first_end_to_end_query_over_fjall() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::new(new_fjall_storage(dir.path()).unwrap()).unwrap();
        create_insert_recursive_join(db);
    }

    #[test]
    fn first_end_to_end_query_over_mem() {
        let db = Db::new(SimStorage::new(7)).unwrap();
        create_insert_recursive_join(db);
    }

    /// `:replace` atomically clears the old rows and inserts the new set,
    /// inside one transaction — the kernel `del_range` and the puts commit
    /// together.
    #[test]
    fn replace_is_atomic_clear_and_insert() {
        let db = Db::new(SimStorage::new(3)).unwrap();
        db.run_script(
            "?[a, b] <- [[1, 2], [2, 3], [3, 4]] :create edge {a, b}",
            no_params(),
        )
        .expect("create");
        db.run_script("?[a, b] <- [[9, 9]] :replace edge {a, b}", no_params())
            .expect("replace");
        let out = db
            .run_script("?[a, b] := *edge[a, b]", no_params())
            .expect("scan");
        // The old three rows are gone; only the replacement survives.
        assert_eq!(int_rows(&out), vec![vec![9, 9]]);
    }

    /// Two sessions racing read-modify-write on one counter row over the real
    /// concurrent backend: each increment reads the counter (putting it in the
    /// conflict set) then writes it back, so colliding commits force one to
    /// take a typed `ConflictError` and replay. If the retry loop worked and no
    /// update was lost, the final value equals the total number of increments.
    #[test]
    fn retry_under_contention_loses_no_update() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::new(new_fjall_storage(dir.path()).unwrap()).unwrap();
        db.run_script("?[k, v] <- [[0, 0]] :create ctr {k => v}", no_params())
            .expect("create counter");

        const PER_THREAD: i64 = 25;
        std::thread::scope(|scope| {
            for _ in 0..2 {
                let db = db.clone();
                scope.spawn(move || {
                    for _ in 0..PER_THREAD {
                        db.run_script(
                            "?[k, v] := *ctr[k, old], v = old + 1 :put ctr {k, v}",
                            no_params(),
                        )
                        .expect("increment");
                    }
                });
            }
        });

        assert_eq!(
            current(&db),
            2 * PER_THREAD,
            "every increment landed; the retry loop lost no update"
        );
    }

    /// The reviewers' refuting scenario, pinned end to end: a retraction
    /// lands at ITS OWN transaction's stamp instant, so it governs over
    /// every earlier claim whatever wall-clock value the script captured
    /// — delete-then-reinsert cycles resolve correctly on the logical-
    /// clock sim backend exactly as on fjall. (The shipped defect keyed
    /// retractions off script wall time while asserts used the stamp; on
    /// the sim's logical clock the domains were incomparable and a plain
    /// delete-reinsert lost the row for the life of the process.)
    #[test]
    fn retraction_governs_across_transactions_on_both_backends() {
        fn drive<S: Storage>(db: Db<S>) {
            db.run_script("?[k, v] <- [[1, 'first']] :create t {k => v}", no_params())
                .expect("create");
            db.run_script("?[k] <- [[1]] :rm t {k}", no_params())
                .expect("rm");
            let gone = db
                .run_script("?[k, v] := *t[k, v]", no_params())
                .expect("read");
            assert!(
                gone.rows.is_empty(),
                "retracted fact must be absent: {gone:?}"
            );
            db.run_script("?[k, v] <- [[1, 'second']] :put t {k => v}", no_params())
                .expect("reinsert");
            let back = db
                .run_script("?[k, v] := *t[k, v]", no_params())
                .expect("read");
            assert_eq!(back.rows.len(), 1, "reinserted fact must be present");
            assert_eq!(back.rows[0][1], DataValue::from("second"));
            // And once more: the second retraction must also govern.
            db.run_script("?[k] <- [[1]] :rm t {k}", no_params())
                .expect("rm again");
            let gone = db
                .run_script("?[k, v] := *t[k, v]", no_params())
                .expect("read");
            assert!(gone.rows.is_empty(), "re-retracted fact must be absent");
        }
        let dir = tempfile::tempdir().unwrap();
        drive(Db::new(new_fjall_storage(dir.path()).unwrap()).unwrap());
        drive(Db::new(crate::storage::sim::SimStorage::new(7)).unwrap());
    }

    /// The language surface's coordinate ORDER, pinned with a
    /// discriminating history (reviewer's probe): a retroactive write —
    /// valid instant far in the past, system stamp now — reads back only
    /// when the parser maps `@ first, second` to (system, valid). Swapped
    /// coordinates put the system cut before the write's stamp, where the
    /// record knew nothing.
    #[test]
    fn asof_clause_first_coordinate_is_system_time() {
        use crate::runtime::relation::get_relation;
        let dir = tempfile::tempdir().unwrap();
        let db = Db::new(new_fjall_storage(dir.path()).unwrap()).unwrap();
        db.run_script(
            "?[k, v] <- [[9, 'seed']] :create hist {k => v}",
            no_params(),
        )
        .expect("create");
        // The retroactive write: valid = 150 µs (ancient), sys = now.
        let mut tx = db.storage.write_tx().unwrap();
        let handle = get_relation(&tx, "hist").unwrap();
        handle
            .put_fact(
                &mut tx,
                &[DataValue::from(1), DataValue::from("retro")],
                crate::data::value::ValidityTs(std::cmp::Reverse(150)),
                SourceSpan(0, 0),
            )
            .unwrap();
        tx.commit().unwrap();
        let now = crate::data::value::current_validity().unwrap().0.0;

        // (sys=now, valid=200): the record NOW says the fact held at 200.
        let rows = db
            .run_script(&format!("?[v] := *hist[1, v @ {now}, 200]"), no_params())
            .expect("two-coordinate read");
        assert_eq!(
            rows.rows,
            vec![vec![DataValue::from("retro")]],
            "system-now, valid-200 must see the retroactive claim"
        );
        // Swapped (sys=200, valid=now): at system time 200 µs the record
        // did not exist; a parser that swapped coordinates would return
        // the row here and the empty set above.
        let rows = db
            .run_script(&format!("?[v] := *hist[1, v @ 200, {now}]"), no_params())
            .expect("swapped-coordinate read");
        assert!(
            rows.rows.is_empty(),
            "at system time 200µs the record knew nothing: {rows:?}"
        );
    }

    /// Index-served as-of reads answer exactly like base scans, through
    /// the REAL `::index create` surface: same rows at every coordinate,
    /// including one where the fact was retracted and one where its value
    /// changed between coordinates.
    #[test]
    fn plain_index_asof_reads_match_base_scans() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::new(new_fjall_storage(dir.path()).unwrap()).unwrap();
        db.run_script(
            "?[k, v] <- [[1, 10], [2, 20]] :create t {k => v}",
            no_params(),
        )
        .expect("create");
        db.run_script("::index create t:by_v {v}", no_params())
            .expect("::index create must be a live surface");
        // History: update k=1, retract k=2 (distinct stamps).
        db.run_script("?[k, v] <- [[1, 11]] :put t {k => v}", no_params())
            .expect("update");
        db.run_script("?[k] <- [[2]] :rm t {k}", no_params())
            .expect("retract");

        // The index-served plan binds v first (the index's leading
        // column); the base plan binds k first. Same logical query.
        let via_index = db
            .run_script("?[v, k] := *t:by_v{v, k} :order v", no_params())
            .expect("index read");
        let via_base = db
            .run_script("?[v, k] := *t[k, v] :order v", no_params())
            .expect("base read");
        assert_eq!(
            via_index.rows, via_base.rows,
            "index and base must agree on current state"
        );
        assert_eq!(via_base.rows.len(), 1, "one row: k=1 updated, k=2 gone");
        assert_eq!(
            via_base.rows[0],
            vec![DataValue::from(11), DataValue::from(1)]
        );
    }

    /// The guard idiom is a language guarantee: `&&`, `||`, and `~`
    /// short-circuit, so a deciding left side protects the right side
    /// from ever evaluating — through the whole engine, not just the
    /// expression unit.
    #[test]
    fn guard_idiom_short_circuits_through_scripts() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::new(new_fjall_storage(dir.path()).unwrap()).unwrap();
        db.run_script(
            "?[k, v] <- [[0, 10], [2, 20]] :create t {k => v}",
            no_params(),
        )
        .expect("create");
        // `%` errors on a zero divisor (`/` does NOT — it yields inf —
        // so a division guard cannot discriminate lazy from strict; the
        // hostile review caught the original test passing vacuously).
        let rows = db
            .run_script("?[k] := *t[k, v], k != 0 && v % k == 0", no_params())
            .expect("guarded modulo must not error on the zero row");
        assert_eq!(int_rows(&rows), vec![vec![2]]);
        // Same law when the connective is nested inside another expression.
        let rows = db
            .run_script(
                "?[k] := *t[k, v], w = if(k != 0 && v % k == 0, 1, 0), w == 1",
                no_params(),
            )
            .expect("nested guard must not error");
        assert_eq!(int_rows(&rows), vec![vec![2]]);
        // The mirror proves the pin has teeth: unguarded, the zero row
        // DOES error.
        db.run_script("?[k] := *t[k, v], v % k == 0", no_params())
            .expect_err("unguarded modulo must error on the zero row");
        // Coalesce guards the same way.
        let rows = db
            .run_script("?[x] := x = null ~ 7", no_params())
            .expect("coalesce");
        assert_eq!(int_rows(&rows), vec![vec![7]]);
    }

    /// The reviewers' pushdown hazard, pinned: `to_conjunction` splits a
    /// top-level guard conjunction across join sides, and the split must
    /// never let the guarded expression evaluate on rows its guard would
    /// have excluded — in any atom order, stored or derived.
    #[test]
    fn guard_survives_conjunction_pushdown_across_joins() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::new(new_fjall_storage(dir.path()).unwrap()).unwrap();
        db.run_script("?[k] <- [[1], [2]] :create a {k}", no_params())
            .expect("create a");
        db.run_script(
            "?[k, v] <- [[0, 5], [1, 20], [2, 30]] :create b {k => v}",
            no_params(),
        )
        .expect("create b");
        for (name, script) in [
            (
                "stored join",
                "?[k, v] := *a[k], *b[k, v], k != 0 && v % k == 0",
            ),
            (
                "reordered",
                "?[k, v] := *b[k, v], *a[k], k != 0 && v % k == 0",
            ),
            (
                "derived sides",
                "aa[k] := *a[k]\nbb[k, v] := *b[k, v]\n?[k, v] := aa[k], bb[k, v], k != 0 && v % k == 0",
            ),
        ] {
            let rows = db
                .run_script(script, no_params())
                .unwrap_or_else(|e| panic!("{name}: guard must survive pushdown: {e}"));
            assert_eq!(
                int_rows(&rows),
                vec![vec![1, 20], vec![2, 30]],
                "{name}: wrong rows"
            );
        }
    }

    /// Backfill batching at scale: a plain index created over MORE rows
    /// than one backfill batch (4096) must index every row exactly once —
    /// the resume bound (fact prefix + `Bot`) must neither skip nor
    /// double-count across batch boundaries.
    #[test]
    fn index_backfill_resumes_correctly_across_batches() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::new(new_fjall_storage(dir.path()).unwrap()).unwrap();
        db.run_script("?[k, v] <- [[0, 0]] :create big {k => v}", no_params())
            .expect("create");
        let mut chunk = vec![];
        for i in 0..5000i64 {
            chunk.push(format!("[{}, {}]", i, i * 7));
            if chunk.len() == 500 {
                db.run_script(
                    &format!("?[k, v] <- [{}] :put big {{k => v}}", chunk.join(", ")),
                    no_params(),
                )
                .expect("seed chunk");
                chunk.clear();
            }
        }
        db.run_script("::index create big:by_v {v}", no_params())
            .expect("index create over 5000 rows");
        let via_index = db
            .run_script("?[count(v)] := *big:by_v{v, k}", no_params())
            .expect("count via index");
        assert_eq!(
            int_rows(&via_index),
            vec![vec![5000]],
            "every row indexed once"
        );
        // And value-level agreement with the base on a spot range.
        let via_base = db
            .run_script(
                "?[v] := *big[k, v], k >= 4094, k <= 4098 :order v",
                no_params(),
            )
            .expect("base spot");
        let via_idx = db
            .run_script(
                "?[v] := *big:by_v{v, k}, k >= 4094, k <= 4098 :order v",
                no_params(),
            )
            .expect("index spot");
        assert_eq!(via_base.rows, via_idx.rows, "batch-boundary rows agree");
    }

    /// The `@` clause parses in both arities through the public script
    /// surface: `@ valid` (current belief) and `@ system, valid` (the
    /// record as it was). Resolution semantics are pinned by the
    /// time-travel trials; this pins the LANGUAGE surface.
    #[test]
    fn asof_clause_parses_one_and_two_coordinates() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::new(new_fjall_storage(dir.path()).unwrap()).unwrap();
        db.run_script("?[k, v] <- [[1, 10]] :create hist {k => v}", no_params())
            .expect("create");
        db.run_script("?[k, v] := *hist[k, v @ 12345]", no_params())
            .expect("single-coordinate as-of parses and runs");
        db.run_script("?[k, v] := *hist[k, v @ 12345, 67890]", no_params())
            .expect("two-coordinate as-of parses and runs");
        db.run_script("?[k, v] := *hist{k, v @ 12345, 67890}", no_params())
            .expect("two-coordinate as-of parses in named form");
    }

    fn current<S: Storage>(db: &Db<S>) -> i64 {
        let out = db
            .run_script("?[v] := *ctr[k, v]", no_params())
            .expect("read counter");
        out.rows[0][0].get_int().expect("int")
    }

    /// A deterministic derived-tuple ceiling refuses a query that would derive
    /// more, through the public API — a typed refusal, reproducibly, with no
    /// wall-clock dependence.
    #[test]
    fn budget_refusal_is_deterministic_and_typed() {
        let db = Db::new(SimStorage::new(5)).unwrap();
        db.run_script(
            "?[a, b] <- [[1, 2], [2, 3], [3, 4], [4, 2]] :create edge {a, b}",
            no_params(),
        )
        .expect("create");

        // The transitive closure derives 12 path tuples; a ceiling of 3 must
        // refuse it, and the refusal must be a value, not a panic.
        let opts = ScriptOptions {
            derived_tuple_ceiling: Some(3),
            ..Default::default()
        };
        let err = db
            .run_script_with(
                "
                path[a, b] := *edge[a, b]
                path[a, b] := *edge[a, c], path[c, b]
                ?[a, b] := path[a, b]
                ",
                no_params(),
                opts,
            )
            .expect_err("must refuse under the ceiling");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("budget") || msg.contains("ceiling") || msg.contains("derived"),
            "expected a budget refusal, got: {msg}"
        );

        // The same query with a generous default runs to completion.
        let ok = db
            .run_script(
                "
                path[a, b] := *edge[a, b]
                path[a, b] := *edge[a, c], path[c, b]
                ?[a, b] := path[a, b]
                ",
                no_params(),
            )
            .expect("default budget completes");
        assert_eq!(ok.rows.len(), 12);
    }

    /// Exercises the normalizer paths the recursive-join test does not: a
    /// stratified negation (`not *edge[b, a]`), which drives negation-normal
    /// form and the binding-safety well-ordering, and a named-field relation
    /// read (`*edge{a: x}`), which drives catalog-schema field resolution.
    #[test]
    fn negation_and_named_field_through_public_api() {
        let db = Db::new(SimStorage::new(13)).unwrap();
        db.run_script(
            "?[a, b] <- [[1, 2], [2, 1], [2, 3], [3, 4], [4, 2]] :create edge {a, b}",
            no_params(),
        )
        .expect("create");

        // Sources of edges whose reverse is absent: 1↔2 is symmetric (both
        // excluded); 2→3, 3→4, 4→2 have no reverse, so their sources qualify.
        let neg = db
            .run_script("?[a] := *edge[a, b], not *edge[b, a]", no_params())
            .expect("negation query");
        assert_eq!(int_rows(&neg), vec![vec![2], vec![3], vec![4]]);

        // Named-field read binds the `a` column by name; the result is every
        // distinct source vertex.
        let named = db
            .run_script("?[x] := *edge{a: x}", no_params())
            .expect("named-field query");
        assert_eq!(int_rows(&named), vec![vec![1], vec![2], vec![3], vec![4]]);
    }

    // ── obligation 11: the magic-sets end-to-end differential ────────────────

    /// The compiled plan's symbols, so a test can prove the magic-sets
    /// rewrite actually fired (a non-`Muggle` symbol) rather than trusting a
    /// bound-recursive query to have triggered it.
    fn compiled_magic_symbols<S: Storage>(db: &Db<S>, script: &str) -> Vec<String> {
        let cur_vld = current_validity().unwrap();
        let fixed = db.fixed_rules();
        let prog = match parse_script(script, &no_params(), &fixed, cur_vld).unwrap() {
            Script::Single(p) => *p,
            _ => panic!("expected a single query"),
        };
        let tx = SessionTx::new_read(db.storage.read_tx().unwrap(), ScriptOptions::default());
        let view = SessionView {
            store: &tx.store,
            temp: &tx.temp,
        };
        let mut normalizer = SessionNormalizer::new(view, CancelFlag::default());
        let (nf, _) = prog.into_normalized_program(&mut normalizer).unwrap();
        let (strat, _lifetimes) = nf.into_stratified_program().unwrap();
        let magic = strat.magic_sets_rewrite(&view).unwrap();
        magic
            .into_strata()
            .into_iter()
            .flat_map(|m| m.prog.into_keys())
            .map(|sym| format!("{sym:?}"))
            .collect()
    }

    /// The last unexercised engine law (query/mod.rs #1, magic-sets half):
    /// **the demand transform changes which rows are computed, never the
    /// result semantics.** Two bound-argument queries against a recursive
    /// rule — the shape where magic rewriting fires — are each asserted equal
    /// to the reference `laws::naive_eval` (which computes the full fixpoint,
    /// no demand restriction) on the same program and facts. The disconnected
    /// `5→6` component makes the demand selective: a rewriter that lost or
    /// leaked demand returns the wrong rows, not merely a slower plan.
    #[test]
    fn magic_sets_demand_matches_naive_oracle_end_to_end() {
        use crate::query::laws::{Literal, Program, Rule, Term, naive_eval};

        let edges = [(1, 2), (2, 3), (3, 4), (5, 6)];
        let var = |s: &'static str| Term::Var(s);
        let lit = |rel: &'static str, args: Vec<Term>| Literal::pos(rel, args);

        // The reference program: path = edge ∪ edge∘path, full fixpoint.
        let program = Program {
            rules: vec![
                Rule::plain(
                    "path",
                    vec![var("a"), var("b")],
                    vec![lit("edge", vec![var("a"), var("b")])],
                ),
                Rule::plain(
                    "path",
                    vec![var("a"), var("b")],
                    vec![
                        lit("edge", vec![var("a"), var("c")]),
                        lit("path", vec![var("c"), var("b")]),
                    ],
                ),
            ],
            facts: [(
                "edge",
                edges
                    .iter()
                    .map(|(a, b)| vec![DataValue::from(*a as i64), DataValue::from(*b as i64)])
                    .collect(),
            )]
            .into_iter()
            .collect(),
            ..Program::default()
        };
        let oracle = naive_eval(&program).expect("reference program evaluates");
        let full_path = &oracle["path"];

        // The same program+facts through the real engine.
        let db = Db::new(SimStorage::new(17)).unwrap();
        db.run_script(
            "?[a, b] <- [[1, 2], [2, 3], [3, 4], [5, 6]] :create edge {a, b}",
            no_params(),
        )
        .expect("create edges");
        let recursive_rule = "
            path[a, b] := *edge[a, b]
            path[a, b] := *edge[a, c], path[c, b]
        ";

        // Demand pattern 1: first argument bound (forward reachability from 1).
        // A rewritten plan carries adorned symbols (`path|Mbf` magic, `path|Ibf`
        // input, `path|S…` supplementary); a Muggle symbol has no `|adornment`.
        let q1 = format!("{recursive_rule}\n?[d] := path[1, d]");
        let syms1 = compiled_magic_symbols(&db, &q1);
        assert!(
            syms1.iter().any(|s| s.contains('|')),
            "the bound-first-arg query must trigger the magic-sets rewrite; symbols were {syms1:?}"
        );
        let got1 = int_rows(&db.run_script(&q1, no_params()).expect("bound-first query"));
        let want1: Vec<Vec<i64>> = {
            let mut v: Vec<Vec<i64>> = full_path
                .iter()
                .filter(|t| t[0] == DataValue::from(1i64))
                .map(|t| vec![t[1].get_int().unwrap()])
                .collect();
            v.sort();
            v.dedup();
            v
        };
        assert_eq!(got1, want1, "forward-demand result must match the oracle");
        assert_eq!(got1, vec![vec![2], vec![3], vec![4]]); // excludes the 5→6 component

        // Demand pattern 2: second argument bound (who reaches 4).
        let q2 = format!("{recursive_rule}\n?[a] := path[a, 4]");
        let syms2 = compiled_magic_symbols(&db, &q2);
        assert!(
            syms2.iter().any(|s| s.contains('|')),
            "the bound-second-arg query must trigger the magic-sets rewrite; symbols were {syms2:?}"
        );
        let got2 = int_rows(&db.run_script(&q2, no_params()).expect("bound-second query"));
        let want2: Vec<Vec<i64>> = {
            let mut v: Vec<Vec<i64>> = full_path
                .iter()
                .filter(|t| t[1] == DataValue::from(4i64))
                .map(|t| vec![t[0].get_int().unwrap()])
                .collect();
            v.sort();
            v.dedup();
            v
        };
        assert_eq!(got2, want2, "backward-demand result must match the oracle");
        assert_eq!(got2, vec![vec![1], vec![2], vec![3]]);
    }
}
