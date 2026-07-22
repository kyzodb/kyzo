/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (`session/db.rs` + `runtime/transact.rs`, MPL-2.0), re-architected for the
 * KyzoDB kernel and session model (story #3):
 *
 * - **Session species.** A session is a [`SessionTx<T>`] owning its backend
 *   transaction `T` and a private scratch store. Mutation lives on
 *   `T: WriteTx` only, so writing through a read session does not compile —
 *   the read/write distinction is a type, not a convention. No `'s`
 *   transaction lifetime is threaded through the engine; the session owns
 *   its transaction and is `Send`.
 * - **Conflict retry.** Every write commit is wrapped by
 *   [`crate::store::retry::retry_on_conflict`]: a `ConflictError` at commit
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
 *   [`crate::exec::fixpoint::eval::Budget`] built from the query's options and the
 *   caller's [`ScriptOptions`]: a deterministic epoch ceiling checked at
 *   epoch barriers, an optional deterministic derived-tuple ceiling, and an
 *   optional wall-clock deadline. There is no cooperative-poison thread and
 *   nothing sleeps to enforce a limit.
 * - **The catalog is typed.** Relation rows are addressed through
 *   `session/catalog.rs`'s [`SystemKey`], and `current_validity()` is
 *   fallible and threaded as `?`.
 *
 * - **Fixed rules run.** Registration (register/unregister/re-exports) and
 *   evaluation are both wired: a query that APPLIES a fixed rule builds the
 *   `FixedRuleEval` adapter ([`crate::rules::contract::SessionFixedRule`])
 *   that bridges `MagicFixedRuleApply` to `FixedRule::run`, sharing the
 *   budget's cancel poll as the rule's `CancelFlag`. This includes the
 *   `Constant` rule behind every `<- [[…]]` inline datum.
 *
 * INTERIM (named, not smoothed over):
 * - Index-operator system ops are LANDED (`::index`, `::hnsw`, `::fts`,
 *   `::lsh` create/drop all dispatch to the real creation/backfill tier);
 *   this note previously deferred them and went stale — an external audit
 *   read the stale claim as ground truth, which is exactly the failure a
 *   comment in this codebase must never cause. Still deferred, still typed:
 *   `::explain` and `::running`/`::kill` (see `IndexOpNotLanded`).
 *   `::verify` is landed on the provenance door (`session/verify.rs` →
 *   `exec/provenance`); root tamper-evidence (#289) is separate.
 * - The imperative script genus (`Script::Imperative`) is refused; the query
 *   and system genera are executed.
 */

//! The Engine admission door: from a script string to result rows.
//!
//! [`Engine`] holds Store and Catalog capabilities by composition — never
//! owns Store doors, never persists. [`Engine::run_script`] parses a script
//! and runs it: a query compiles (normalize → stratify → magic-sets →
//! relational-algebra plan) and evaluates semi-naively over the session's
//! transaction, a mutation additionally writes its result set back through
//! the mutation pipeline, and a system op reads or edits the catalog. The
//! result is a [`NamedRows`]. The fused `Db` / `Db::new(storage)` ambient
//! bag is deleted (decisions.md §1).

// Carried obligation: clippy-collapsible-if-drift — record at this seat.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use miette::{Diagnostic, Result, bail};
use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

use crate::data::json::NamedRows;
use crate::exec::fixpoint::delta_store::{EpochStore, TupleInIter};
use crate::exec::fixpoint::eval::{Budget, RowLimit, stratified_evaluate};
use crate::exec::plan::compile::stratified_magic_compile;
use crate::exec::plan::magic::StoredRelationSchemaSource;
use crate::exec::sort::sort_and_collect;
use crate::parse::sys::SysOp;
use crate::parse::{Script, parse_script};
use crate::rules::contract::SessionFixedRule;
use crate::rules::contract::{
    CancelAuthority, CancelFlag, DEFAULT_FIXED_RULES, FixedRule, StoredInputSource,
};
use crate::session::catalog::{
    Catalog, ConstraintRef, KeyspaceKind, RelationHandle, Residency, create_relation,
    destroy_relation, get_relation, write_relation_row,
};
use crate::session::current_validity;
pub(crate) use crate::session::normalize::SessionNormalizer;
use crate::session::observe::{CallbackCollector, EventCallbackRegistry};
use crate::store::idempotency::IdempotencyMemo;
use crate::store::retry::RetryError;
use crate::store::scratch::TempTx;
use crate::store::sweep::{
    LiveSweepHandle, SingleStoreKeyPreimage, current_live_sweep, install_live_sweep,
};
use crate::store::{
    CommitFailure, CommitIo, ReadTx, Storage, SweepRefuse, SweepSealFailure, WriteTx,
};
use kyzo_model::SourceSpan;
use kyzo_model::program::symbol::Symbol;
use kyzo_model::program::{
    InputProgram, QueryAssertion, QueryOutOptions, RelationOp, ReturnMutation,
};
use kyzo_model::schema::StoredRelationMetadata;
use kyzo_model::value::Tuple;
use kyzo_model::value::row::TupleIter;
use kyzo_model::value::{AsOf, DataValue, ValidityTs};

/// The deterministic default ceiling on evaluation epochs (semi-naive
/// iterations). High enough for real recursion over finite data; bounds a
/// runaway fixpoint into a typed refusal at an epoch barrier rather than an
/// unbounded loop. Overridable per script through [`ScriptOptions`].
pub(crate) const DEFAULT_EPOCH_CEILING: u32 = 1_000_000;

/// How many times a write commit replays on a typed [`ConflictError`]
/// before giving up. Reads never conflict. This is a liveness backstop
/// against pathological contention, not a tuning knob: with the retry
/// tier's capped exponential backoff, exhausting it means roughly eight
/// seconds of continuous same-fact races — at 32 it was reachable by
/// three writers under a loaded machine, which is contention working,
/// not failing.
///
/// [`ConflictError`]: crate::store::ConflictError
const MAX_COMMIT_ATTEMPTS: NonZeroUsize = NonZeroUsize::new(128).unwrap();

/// Closed Engine admission refuse taxonomy (decisions.md §42/§43).
///
/// The seven production panic-shapes that lived as separate structs on the
/// fused `Db` bag are named variants of one enum at the single Engine
/// admission door. Store debt/backpressure stays exclusive to
/// `store/failure.rs` (`StoreRefuse`) — never merged here.
#[derive(Debug, Error, Diagnostic)]
pub(crate) enum EngineRefuse {
    /// A script asked for the imperative genus (`?[…] <- …` control flow),
    /// which the session tier executes for queries and system ops but not
    /// yet for imperative blocks.
    #[error("imperative scripts are not executed yet")]
    #[diagnostic(code(db::imperative_not_wired))]
    ImperativeNotWired,

    /// A store op targeting a temp relation (`_`-prefixed). In this tier the
    /// session — and with it the temp store — lives exactly as long as one
    /// script, so a temp write could never be observed; without this refusal
    /// the read path would silently drop the mutation (review finding F2).
    /// Lands for real with multi-script sessions.
    #[error("temp relation '{0}' cannot be stored to yet: sessions do not outlive a script")]
    #[diagnostic(code(db::temp_relation_not_reachable))]
    #[diagnostic(help(
        "temp relations (`_`-prefixed) become writable when multi-script \
         sessions land; store to a named relation instead"
    ))]
    TempRelationNotReachableError(String, #[label] kyzo_model::SourceSpan),

    /// A system op needs an operator-tier feature that has not landed — today
    /// exactly `::explain` and `::running`/`::kill`. Index DDL (`::index`,
    /// `::hnsw`, `::fts`, `::lsh` create/drop) and catalog ops are complete;
    /// this error's name predates their landing and survives only for the two
    /// ops above.
    #[error("system op '{0}' needs the index-operator tier, which has not landed")]
    #[diagnostic(code(db::index_op_not_landed))]
    IndexOpNotLanded(&'static str),

    /// A `:assert none` / `:assert some` query option was violated.
    #[error("{0}")]
    #[diagnostic(code(db::assertion_failure))]
    QueryAssertionFailure(String, #[label] kyzo_model::SourceSpan),

    /// Registering a fixed rule under a name already taken.
    #[error("cannot register fixed rule '{0}': the name is already taken")]
    #[diagnostic(code(db::fixed_rule_name_conflict))]
    FixedRuleNameConflict(String),

    /// A mutation named an output relation whose precondition the op requires:
    /// `:create` on an existing relation, or a non-create/replace op on a
    /// missing one.
    #[error("{0}")]
    #[diagnostic(code(db::store_relation_precondition))]
    StoreRelationPrecondition(String),

    /// A `:timeout` (or caller-supplied deadline) that cannot become a
    /// [`Duration`]: negative, non-finite, or too large to fit. The parser only
    /// bounds `:timeout` by `> 0`, so this is the last line of defense before
    /// `Duration::from_secs_f64` would panic.
    #[error(
        "timeout of {0} seconds is not usable: it must be a finite, non-negative number of seconds that fits in a Duration"
    )]
    #[diagnostic(code(db::invalid_timeout))]
    InvalidTimeout(f64),
}

/// The deterministic default ceiling on total derived tuples admitted across
/// one query (`eval::BudgetDimension::DerivedTuples`, summed over EVERY
/// store the query touches — a plain `?[x] := r[x]` entry rule copies its
/// source's admissions into the output store too, so a query's true spend is
/// commonly ~2x the size of its "answer"), applied when
/// [`ScriptOptions::derived_tuple_ceiling`] is `None`. Closes the gap a live
/// server hit: a value-generating recursion with no fixpoint (e.g.
/// `f[x] := x = 1; f[x] := f[y], x = y + 1`) was bounded ONLY by
/// [`DEFAULT_EPOCH_CEILING`], and a rule whose OUTPUT WIDENS per epoch (a
/// join that fans out, not merely a slow successor chain) can exhaust memory
/// in a handful of epochs — far before any epoch ceiling would ever fire,
/// since that ceiling bounds iteration count, not per-iteration volume.
///
/// `50_000_000` is not a round guess:
/// - it is the EXACT ceiling `bench_api.rs`'s own `generous_budget()`
///   already arms for bulk bench workloads in this engine — reused, not
///   reinvented;
/// - it is verified against this engine's own real-world ceiling: the
///   `kyzo-bench` sibling lane's actual runner
///   (`benches/datalog/kyzo-runner/src/main.rs`) calls `Db::run_script` with
///   NO `ScriptOptions` override at all, so every recorded datalog result
///   ran (and must keep running) under exactly this default. The largest
///   already-published, real-graph result is `tc/snap-p2p-Gnutella08`
///   (6.3k nodes, 20.8k real edges): 13_148_244 answer rows, ~26.3M true
///   spend after the entry-copy doubling — `50_000_000` clears it with
///   ~1.9x headroom, and clears `tc/snap-wiki-Vote`'s 11_947_132 rows
///   (~23.9M spend) the same way. A smaller "fast-refusing" ceiling was
///   considered and rejected: it would have silently regressed these
///   exact already-recorded benchmarks — a real terminating query on a
///   real graph, exactly what "zero friction" must protect;
/// - it is still ~14,000x the largest volume this codebase's OWN test
///   suite derives under default options (a few thousand rows), so it adds
///   no friction there either.
///
/// A query that never reaches a fixpoint (admits at least one net-new tuple
/// every epoch, by definition) is *structurally guaranteed* to cross this
/// ceiling — and be refused with a named, typed dimension — at or before
/// [`DEFAULT_EPOCH_CEILING`]'s own limit, so no such query can run
/// unbounded; a WIDENING one (many new tuples per epoch) crosses it within a
/// handful of epochs, catching the class an epoch ceiling alone cannot
/// bound at all. Overridable per script through
/// [`ScriptOptions::derived_tuple_ceiling`]; a caller with a genuinely
/// larger legitimate workload raises it explicitly.
pub(crate) const DEFAULT_DERIVED_TUPLE_CEILING: u64 = 50_000_000;

/// Per-script evaluation controls. Default is "run to the fixpoint within
/// the deterministic epoch and derived-tuple ceilings, no deadline". These
/// are the knobs that turn a budget into a refusal; they are deterministic
/// (epoch/derived-tuple ceilings) except the wall-clock `timeout`.
#[derive(Clone, Debug)]
pub struct ScriptOptions {
    /// Override the epoch (semi-naive iteration) ceiling. `None` uses
    /// [`DEFAULT_EPOCH_CEILING`].
    pub epoch_ceiling: Option<u32>,
    /// A deterministic ceiling on the number of derived tuples. `None` uses
    /// [`DEFAULT_DERIVED_TUPLE_CEILING`] — never unbounded, so a
    /// value-generating recursion that never reaches a fixpoint always
    /// refuses instead of running away. Refusal is exact and reproducible.
    pub derived_tuple_ceiling: Option<u64>,
    /// A wall-clock deadline in seconds. `None` is no deadline. The query's
    /// own `:timeout` option, if smaller, wins.
    pub timeout_secs: Option<f64>,
    /// Client-durable operation identity for safe-retry (§38). When set,
    /// [`SessionTx::commit_write`] admits under this key; retries with the
    /// same bytes dedupe to one committed effect.
    pub client_operation_id: Option<Vec<u8>>,
    /// Live SweepDoor for this Engine. Default pulls the process-current door
    /// installed at [`Engine::compose`] so constraint/sys paths that build
    /// `ScriptOptions::default()` still hit the OperationKey ack path.
    /// `pub` so out-of-crate FRU (`..ScriptOptions::default()`) can construct
    /// overrides without naming every field.
    pub sweep: Option<LiveSweepHandle>,
}

impl Default for ScriptOptions {
    fn default() -> Self {
        Self {
            epoch_ceiling: None,
            derived_tuple_ceiling: None,
            timeout_secs: None,
            client_operation_id: None,
            sweep: current_live_sweep(),
        }
    }
}

/// Engine: sole Record admission + evaluation + projection orchestration.
///
/// Holds Store (`S: Storage`) and [`Catalog`] capabilities by composition
/// only — never owns Store doors, never persists. Cloning shares the same
/// Store handle, Catalog capability, and registries (behind `Arc`), so
/// callbacks and fixed rules registered on one clone are visible on the
/// others.
///
/// The fused `Db` / `Db::new(storage)` ambient bag is deleted; callers must
/// supply Store and Catalog explicitly via [`Engine::compose`].
pub struct Engine<S> {
    pub store: S,
    pub(crate) catalog: Catalog,
    pub(crate) segments: Arc<crate::project::current::SegmentEngine>,
    pub(crate) fixed_rules: Arc<RwLock<BTreeMap<String, Arc<dyn FixedRule>>>>,
    pub(crate) event_callbacks: Arc<RwLock<EventCallbackRegistry>>,
    pub(crate) callback_count: Arc<AtomicU32>,
    /// Live StoreId / WriteAuthority token / RootChain for admission mint.
    pub(crate) admission: crate::session::admit::LiveAdmissionSeats,
    /// One live SweepDoor per Store (IdempotencyMemo) — opened at compose.
    pub(crate) sweep: LiveSweepHandle,
}

impl<S: Clone> Clone for Engine<S> {
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
            catalog: self.catalog.clone(),
            segments: self.segments.clone(),
            fixed_rules: self.fixed_rules.clone(),
            event_callbacks: self.event_callbacks.clone(),
            callback_count: self.callback_count.clone(),
            admission: self.admission.clone(),
            sweep: self.sweep.clone(),
        }
    }
}

impl<S: Storage> Engine<S> {
    /// Compose an Engine from Store and Catalog capabilities.
    ///
    /// Not `new(storage)`: that fused constructor is deleted. Engine never
    /// mints Catalog from Store alone — both capabilities are required.
    /// Live admission seats are genesis-minted here so Stored sugar can
    /// mint certificates without placeholders. Opens the one live
    /// [`LiveSweepHandle`] for this Store's OperationKey ack path.
    pub fn compose(store: S, catalog: Catalog) -> Result<Self> {
        let fixed_rules = DEFAULT_FIXED_RULES.clone();
        let admission = crate::session::admit::LiveAdmissionSeats::mint_genesis();
        let sweep = LiveSweepHandle::open_for_store(admission.store_id())
            .map_err(|e| miette::miette!("live SweepDoor open refused at Engine::compose: {e}"))?;
        install_live_sweep(sweep.clone());
        Ok(Self {
            store,
            catalog,
            segments: Arc::new(crate::project::current::SegmentEngine::default()),
            fixed_rules: Arc::new(RwLock::new(fixed_rules)),
            event_callbacks: Arc::new(RwLock::new(EventCallbackRegistry::default())),
            callback_count: Arc::new(AtomicU32::new(0)),
            admission,
            sweep,
        })
    }

    /// Bind this Engine's live SweepDoor (+ optional client op id) onto options.
    pub(crate) fn bind_write_options(&self, mut options: ScriptOptions) -> ScriptOptions {
        options.sweep = Some(self.sweep.clone());
        options
    }

    /// Open Store identity sealed into this Engine's live admission seats.
    pub(crate) fn store_id(&self) -> crate::store::open::StoreId {
        self.admission.store_id()
    }

    /// Live certificate inputs: seats + segment CatalogGeneration for `relation`.
    pub(crate) fn live_certificate_inputs(
        &self,
        tx: &impl crate::store::ReadTx,
        relation: kyzo_model::value::RelationId,
    ) -> crate::session::admit::LiveCertificateInputs {
        use crate::session::generation::{CatalogGeneration, RelationGeneration};
        let generation = self.segments.witness_after_snapshot(tx, relation);
        let catalog_generation =
            CatalogGeneration::from_relation(RelationGeneration::witness(generation.raw()));
        self.admission.certificate_inputs(catalog_generation)
    }

    /// The interpretive Catalog capability this Engine holds.
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// A snapshot of the fixed-rule registry: the built-ins plus every
    /// user-registered rule. Handed to the parser (which resolves fixed-rule
    /// names) and to the mutation pipeline (for trigger parsing).
    pub fn fixed_rules(&self) -> BTreeMap<String, Arc<dyn FixedRule>> {
        self.fixed_rules
            .read()
            .expect("fixed-rule registry poisoned")
            .clone()
    }

    /// Register a custom fixed rule under `name`. Errors if the name is taken
    /// (including by a built-in). The rule becomes usable in every session of
    /// this Engine (and its clones).
    pub fn register_fixed_rule(&self, name: String, rule: impl FixedRule + 'static) -> Result<()> {
        let mut registry = self
            .fixed_rules
            .write()
            .expect("fixed-rule registry poisoned");
        if registry.contains_key(&name) {
            bail!(EngineRefuse::FixedRuleNameConflict(name));
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
            bail!(EngineRefuse::FixedRuleNameConflict(name));
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

    /// The next callback registration id (monotonic per Engine).
    pub(crate) fn next_callback_id(&self) -> u32 {
        self.callback_count.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Tamper-evidence door: recompute the store content root and compare
    /// it to this Engine's live [`RootChain`] tip at `cut`.
    pub fn verify_root_chain(
        &self,
        cut: crate::store::sweep::CommitOrdinal,
        budget: std::num::NonZeroU64,
    ) -> Result<crate::session::verify::RootVerifyOutcome> {
        let tx = self.store.read_tx()?;
        let chain = self.admission.root_chain();
        crate::session::verify::verify(&tx, &chain, cut, budget)
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
        let options = self.bind_write_options(options);
        let cur_vld = current_validity()?;
        match parse_script(payload, &params, cur_vld)? {
            Script::Query(prog) => self.execute_single(prog, cur_vld, &options),
            // The parsed `::…` script validated as syntax; the engine-typed
            // lift (`crate::parse::sys`) admits it into a `SysOp` — sealing
            // index configs and admitting tokenizers — before dispatch. The
            // whole payload is the sys script, so it re-parses cleanly.
            Script::Sys(_) => {
                let op =
                    crate::parse::sys::lift(crate::parse::parse_sys(payload, &params, cur_vld)?)?;
                self.run_sys_op(op, cur_vld, &options)
            }
            Script::Imperative(_) => bail!(EngineRefuse::ImperativeNotWired),
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
            bail!(EngineRefuse::TempRelationNotReachableError(
                h.name.name.to_string(),
                h.span
            ));
        }
        if program.needs_write_lock().is_some() {
            let callback_targets = self.current_callback_targets();
            crate::store::retry::retry_on_conflict_with_backoff(MAX_COMMIT_ATTEMPTS, || {
                // Fresh transaction AND fresh collector per attempt: a
                // conflicted attempt is discarded whole, so no phantom events.
                let mut collector = CallbackCollector::default();
                let mut tx = SessionTx::new_write(
                    crate::store::retry::write_tx_attempt(&self.store)?,
                    self.bind_write_options(options.clone()),
                );
                let rows = match self.run_query(
                    &mut tx,
                    program.clone(),
                    cur_vld,
                    &callback_targets,
                    &mut collector,
                    0,
                ) {
                    Ok(rows) => rows,
                    Err(e) => {
                        tx.abort_write();
                        return Err(RetryError::session_report(e));
                    }
                };
                // Integrity constraints: the denial check. Every constraint
                // of every relation this transaction mutated (user writes
                // and trigger writes alike) is evaluated against the
                // post-write state; a non-empty result is a typed refusal
                // and the whole transaction rolls back.
                if let Err(e) = self.enforce_constraints(&mut tx, cur_vld) {
                    tx.abort_write();
                    return Err(RetryError::session_report(e));
                }
                // Segment soundness: bumps precede the commit, so any
                // snapshot that can see these writes sees the new generation.
                for rel in &tx.touched_relations {
                    self.segments.bump_before_commit(*rel);
                }
                let retired = std::mem::take(&mut tx.retired_relations);
                if let Err(e) = tx.commit_write() {
                    return Err(RetryError::from(e));
                }
                // Post-commit only: retirements are durable, so their
                // segments and generation slots leave the engine now (a
                // rolled-back destroy never reaches this line).
                for rel in &retired {
                    self.segments.evict(*rel);
                }
                // The universe is durable, now tell observers.
                self.send_callbacks(collector);
                Ok(rows)
            })
        } else {
            let mut tx = SessionTx::new_read(self.store.read_tx()?, options.clone());
            self.run_query_readonly(&mut tx, program, cur_vld)
        }
    }

    // ─────────────────────────────────────────────────────────────────────
    // The query pipeline
    // ─────────────────────────────────────────────────────────────────────

    /// Compile a program against the session's read surface and evaluate it
    /// semi-naively, returning the raw result store, the entry head, and the
    /// output options. This is the read-only heart shared by every path
    /// (including constraint enforcement, `session/constraint.rs`).
    pub(crate) fn compile_and_eval<T: ReadTx>(
        &self,
        store: &T,
        temp: &TempTx,
        program: InputProgram,
        cur_vld: ValidityTs,
        options: &ScriptOptions,
        segments: crate::project::current::Segments<'_>,
    ) -> Result<(EpochStore, bool, Vec<Symbol>, QueryOutOptions)> {
        let view = SessionView { store, temp };
        let out_opts = program.out_opts().clone();
        let head = program.get_entry_out_head_or_default()?;

        // One cancel lifecycle shared by the budget (checked at epoch
        // barriers), every fixed rule's `CancelFlag` (checked inside long
        // algorithms), and every search atom (checked once per search
        // invocation), so a cancelled or deadline-exceeded query stops
        // them all. `_auth` is retained for a future `::kill` door.
        let (_auth, cancel) = CancelAuthority::arm();

        let mut normalizer = SessionNormalizer::new(view, cancel.clone());
        let (nf, _) =
            crate::exec::plan::program::into_normalized_program(program, &mut normalizer)?;
        let (strat, lifetimes) = nf.into_stratified_program()?;
        let magic = strat.magic_sets_rewrite(&view)?;
        let compiled = stratified_magic_compile(store, magic)?;
        // ONE machine: vectorized execution end to end, judged by the
        // naive oracle. (The row-at-a-time twin was deleted; criterion on
        // a loaded 32-core box had it losing or tying everywhere it was
        // measured against the batch pipeline.)
        let eval_prog =
            crate::exec::plan::compile::bind_for_eval(&compiled, store, segments, &mut |app| {
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

        match cur_vld {
            value => core::mem::drop(value),
        }
        let budget = build_budget(options, &out_opts, cancel)?;
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
                    .early_returned_iter()?
                    .map(TupleInIter::try_into_tuple)
                    .collect::<Result<Vec<_>, _>>()?
            } else {
                result
                    .all_iter()?
                    .map(TupleInIter::try_into_tuple)
                    .collect::<Result<Vec<_>, _>>()?
            }
        } else {
            let sorted = sort_and_collect(result, &out_opts.sorters, head)?;
            let skip = match out_opts.offset {
                Some(v) => v,
                None => 0,
            };
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
                        bail!(EngineRefuse::QueryAssertionFailure(
                            format!(
                                "the query is required to return no rows, but it returned {first:?}"
                            ),
                            *span,
                        ));
                    }
                }
                QueryAssertion::AssertSome(span) => {
                    if rows.is_empty() {
                        bail!(EngineRefuse::QueryAssertionFailure(
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
    /// ([`crate::session::admit::MAX_TRIGGER_CASCADE_DEPTH`]).
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
                    bail!(EngineRefuse::StoreRelationPrecondition(format!(
                        "cannot :create relation '{}': it already exists",
                        meta.name.name
                    )));
                }
                RelationOp::Create | RelationOp::Replace => {}
                _other if !exists => {
                    bail!(EngineRefuse::StoreRelationPrecondition(format!(
                        "relation '{}' does not exist",
                        meta.name.name
                    )));
                }
                RelationOp::Put
                | RelationOp::Insert
                | RelationOp::Update
                | RelationOp::Rm
                | RelationOp::Delete
                | RelationOp::Ensure
                | RelationOp::EnsureNot => {}
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
            crate::project::current::Segments::OFF,
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
                    returning_rows(callback_collector, &meta.name.name)
                } else {
                    Ok(status_ok())
                }
            }
            None => materialize(rows, &head),
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
            crate::project::current::Segments(Some(&self.segments)),
        )?;
        let rows = Self::finalize_rows(&result, limited, &head, &out_opts)?;
        materialize(rows, &head)
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
            // Integrity constraints (session/constraint.rs).
            SysOp::CreateConstraint(name, source) => {
                self.sys_create_constraint(&name, &source, cur_vld, options)
            }
            SysOp::RemoveConstraint(name) => self.sys_remove_constraint(&name),
            SysOp::ListConstraints => self.sys_list_constraints(),

            // Catalog (session/catalog.rs).
            SysOp::ListRelations => self.sys_list_relations(),
            SysOp::ListColumns(name) => self.sys_list_columns(&name),
            SysOp::ListFixedRules => self.sys_list_fixed_rules(),
            SysOp::ShowTrigger(name) => self.sys_show_trigger(&name),
            SysOp::RemoveRelation(names) => self.sys_remove_relation(names),
            SysOp::RenameRelation(pairs) => self.sys_rename_relation(pairs),
            SysOp::DescribeRelation(name, desc) => self.sys_describe_relation(&name, &desc),
            SysOp::SetTriggers(name, puts, rms, replaces) => {
                self.sys_set_triggers(name, puts, rms, replaces)
            }
            SysOp::SetAccessLevel(names, level) => self.sys_set_access_level(names, level),

            // Index / operator lifecycle (session/ops.rs).
            SysOp::Compact => self.sys_compact(),
            SysOp::MerkleRoot(rel) => self.sys_merkle_root(rel.as_ref(), options),
            SysOp::ListIndices(name) => self.sys_list_indices(&name),
            SysOp::CreateIndex(rel, name, cols) => self.sys_create_index(&rel, &name, &cols),
            SysOp::CreateVectorIndex(cfg) => self.sys_create_vector_index(&cfg),
            SysOp::CreateFtsIndex(cfg) => self.sys_create_fts_index(&cfg),
            SysOp::CreateMinHashLshIndex(cfg) => self.sys_create_minhash_lsh_index(&cfg),
            SysOp::RemoveIndex(rel, idx) => self.sys_remove_index(&rel, &idx),

            // Jobs / verify (already delegated).
            SysOp::ListRunning => {
                crate::session::jobs::run_job_op(crate::session::jobs::JobSysOp::ListRunning)
            }
            SysOp::KillRunning(pid) => crate::session::jobs::run_job_op(
                crate::session::jobs::JobSysOp::KillRunning { pid: pid.get() },
            ),
            // Query-answer `::verify` — provenance door (session/verify.rs).
            SysOp::Verify(prog) => {
                let outcome = self.verify_input_program(*prog, cur_vld, options)?;
                Ok(outcome.into_named_rows())
            }
            SysOp::Explain(_) => bail!(EngineRefuse::IndexOpNotLanded("::explain")),
        }
    }

    /// Run a catalog-mutating closure inside a write session, committing with
    /// conflict retry. Shared by catalog and index sys-ops.
    pub(crate) fn sys_write(
        &self,
        f: impl Fn(&mut SessionTx<S::WriteTx>) -> Result<NamedRows>,
    ) -> Result<NamedRows> {
        crate::store::retry::retry_on_conflict_with_backoff(MAX_COMMIT_ATTEMPTS, || {
            let mut tx = SessionTx::new_write(
                crate::store::retry::write_tx_attempt(&self.store)?,
                self.bind_write_options(ScriptOptions::default()),
            );
            let out = match f(&mut tx) {
                Ok(out) => out,
                Err(e) => {
                    tx.abort_write();
                    return Err(RetryError::session_report(e));
                }
            };
            for rel in &tx.touched_relations {
                self.segments.bump_before_commit(*rel);
            }
            let retired = std::mem::take(&mut tx.retired_relations);
            tx.commit_write().map_err(RetryError::from)?;
            for rel in &retired {
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
pub(crate) fn build_budget(
    options: &ScriptOptions,
    out_opts: &QueryOutOptions,
    cancel: CancelFlag,
) -> Result<Budget> {
    let ceiling = match options.epoch_ceiling {
        Some(v) => v,
        None => DEFAULT_EPOCH_CEILING,
    };
    let ceiling = NonZeroU32::new(ceiling.max(1)).expect("max(1) is nonzero");
    let mut budget = Budget::new(ceiling).with_cancel(cancel);
    let derived_tuple_ceiling = match options.derived_tuple_ceiling {
        Some(v) => v,
        None => DEFAULT_DERIVED_TUPLE_CEILING,
    };
    budget = budget.with_derived_tuple_ceiling(derived_tuple_ceiling);
    // The tighter of the caller's deadline and the query's own :timeout.
    let deadline = [options.timeout_secs, out_opts.timeout]
        .into_iter()
        .flatten()
        .filter(|s| *s > 0.0)
        .min_by(|a, b| match a.partial_cmp(b) {
            Some(ord) => ord,
            None => std::cmp::Ordering::Equal,
        });
    if let Some(secs) = deadline {
        let duration =
            Duration::try_from_secs_f64(secs).map_err(|_| EngineRefuse::InvalidTimeout(secs))?;
        budget = budget.with_timeout(duration);
    }
    Ok(budget)
}

/// Materialize a row vector into a headered result.
fn materialize(rows: Vec<Tuple>, head: &[Symbol]) -> Result<NamedRows> {
    let headers = head.iter().map(|s| s.name.to_string()).collect();
    Ok(NamedRows::try_new(headers, rows)?)
}

/// A one-cell `status: OK` result for ops that report success, not rows.
/// Width match is by construction via [`NamedRows::status_ok`].
pub(crate) fn status_ok() -> NamedRows {
    NamedRows::status_ok()
}

/// Build the `:returning` result from what the mutation collected. The
/// collector holds `(op, new, old)` events; `:returning` reports the new rows.
fn returning_rows(collector: &CallbackCollector, relation: &str) -> Result<NamedRows> {
    let mut headers = vec![];
    let mut rows = vec![];
    if let Some(events) = collector.get(relation) {
        for (_op, new, _old) in events {
            if headers.is_empty() {
                headers = new.headers().to_vec();
            }
            rows.extend(new.rows().iter().cloned());
        }
    }
    Ok(NamedRows::try_new(headers, rows)?)
}

// ─────────────────────────────────────────────────────────────────────────
// The session transaction
// ─────────────────────────────────────────────────────────────────────────

/// One session: a backend transaction `T` plus a private scratch store for
/// temp (`_`-prefixed) relations.
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
        crate::session::ops::IndexCtx,
    >,
    pub store: T,
    pub temp: TempTx,
    /// The evaluation controls for every query in this session, including
    /// triggers (which run under the parent's budget).
    pub(crate) options: ScriptOptions,
    /// Integrity constraints of every relation this transaction has
    /// mutated, `name → typed [`ConstraintRef`] substance`, deduped by name
    /// (each relation in a constraint's read-set mirrors the identical
    /// spec). Collected by the mutation pipeline and drained by
    /// [`Engine::enforce_constraints`](crate::session::db::Engine) before commit.
    pub(crate) pending_constraints: BTreeMap<SmartString<LazyCompact>, ConstraintRef>,
    /// Every relation id this transaction wrote (user writes, trigger
    /// writes, index backfills alike) — drained into segment-generation
    /// bumps BEFORE the storage commit (the segments' soundness rule).
    pub(crate) touched_relations: std::collections::BTreeSet<kyzo_model::value::RelationId>,
    /// Relation ids permanently retired by this transaction (destroy /
    /// replace / index drop) — drained into segment-engine evictions
    /// AFTER a successful commit (a rolled-back destroy retires nothing).
    pub(crate) retired_relations: std::collections::BTreeSet<kyzo_model::value::RelationId>,
}

impl<T: ReadTx> SessionTx<T> {
    pub fn new_read(store: T, options: ScriptOptions) -> Self {
        Self {
            store,
            temp: TempTx::default(),
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

    /// The fact's LOGICAL row governing AT `valid`, routed: the versioned
    /// format's point read (a bitemporal probe under the fact's key
    /// prefix, resolved with the newest system knowledge), replacing
    /// exact-key reads for relation rows.
    ///
    /// `valid` is the write's OWN target instant — "the row this write
    /// supersedes" must mean "whatever governed the instant being
    /// written", not some unrelated later instant a different write
    /// happened to land at. The three write paths in `session/admit.rs`
    /// pass their own resolved `WriteValidity` coordinate; `:ensure` /
    /// `:ensure_not` (which can never carry a `@` clause) pass
    /// [`kyzo_model::value::MAX_VALIDITY_TS`] for the ordinary "does this
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
        match handle.residency() {
            Residency::Temp => handle.current_row(&self.temp, key_cols, as_of, span),
            Residency::Stored => handle.current_row(&self.store, key_cols, as_of, span),
        }
    }
}

impl<T: WriteTx> SessionTx<T> {
    pub(crate) fn new_write(store: T, options: ScriptOptions) -> Self {
        Self {
            store,
            temp: TempTx::default(),
            index_ctxs: BTreeMap::new(),
            options,
            pending_constraints: BTreeMap::new(),
            touched_relations: std::collections::BTreeSet::new(),
            retired_relations: std::collections::BTreeSet::new(),
        }
    }

    /// Spend both the persistent write tx and the session scratch Open —
    /// error / rollback path. Consumes `self` so Drop cannot bomb either.
    pub(crate) fn abort_write(self) {
        let SessionTx {
            store, mut temp, ..
        } = self;
        temp.discard();
        match store.abort() {
            crate::store::tx::Aborted => {}
        }
    }

    /// Commit the persistent write tx and spend the session scratch Open.
    /// Consumes `self` so Drop cannot bomb either side.
    ///
    /// #375 T1 / seat 25: routes through the Engine's live
    /// [`LiveSweepHandle`] — `admit(OperationKey, RequestDigest)` →
    /// StableCommitCap NativeFsyncProof barrier → terminal IdempotencyMemo —
    /// never key-less [`SweepDoor::ack_native_fsync_barrier`] alone, never bare
    /// non-fsync [`WriteTx::commit`]. Same client operation identity retries
    /// dedupe to one committed effect (incl. after WAL memo restore).
    pub(crate) fn commit_write(self) -> std::result::Result<(), CommitFailure> {
        let SessionTx {
            store,
            mut temp,
            options,
            ..
        } = self;
        let sweep = options
            .sweep
            .clone()
            .or_else(current_live_sweep)
            .ok_or(CommitFailure::Io(CommitIo::DurableAckArmRefused))?;
        let store_id = sweep.store_id();
        let client_op = match options.client_operation_id.clone() {
            Some(id) => id,
            None => sweep.next_anon_operation_id(),
        };
        let preimage = SingleStoreKeyPreimage {
            domain_label: b"kyzo.script.write".to_vec(),
            client_operation_id: client_op.clone(),
            step_id: b"commit_write".to_vec(),
        };
        let key = preimage.derive_key(store_id);
        let request_digest = IdempotencyMemo::digest_request(&client_op);
        let sealed = sweep.with_mut(|door, session, incarnation| {
            door.ack_write(incarnation, session, key, request_digest, preimage, store)
        });
        temp.discard();
        match sealed {
            Ok(()) => Ok(()),
            Err(SweepSealFailure::Apply(f)) => Err(f),
            Err(SweepSealFailure::Sweep(SweepRefuse::NativeFsyncAckArmRequired))
            | Err(SweepSealFailure::Sweep(SweepRefuse::OperationKeyReuse)) => {
                Err(CommitFailure::Io(CommitIo::DurableAckArmRefused))
            }
            Err(SweepSealFailure::Sweep(other)) => {
                debug_assert!(
                    false,
                    "ack_write returned unexpected SweepRefuse: {other:?}"
                );
                Err(CommitFailure::Io(CommitIo::DurableAckArmRefused))
            }
            Err(SweepSealFailure::MerkleChain(_)) | Err(SweepSealFailure::Wal(_)) => {
                Err(CommitFailure::Io(CommitIo::DurableAckArmRefused))
            }
        }
    }

    /// The system stamp every bitemporal row written to the given store
    /// carries, routed like the row itself: the persistent store's stamp
    /// comes from the storage's monotone clock, the session temp store's
    /// from its logical clock. One stamp per transaction per store — a
    /// transaction's writes are one instant of recorded history.
    pub(crate) fn system_stamp_routed(&self, residency: Residency) -> ValidityTs {
        match residency {
            Residency::Temp => self.temp.system_stamp(),
            Residency::Stored => self.store.system_stamp(),
        }
    }

    /// Register a mutated relation's integrity constraints for the
    /// pre-commit denial check. Idempotent per constraint name: the same
    /// constraint mirrored on several touched relations is checked once.
    pub(crate) fn note_constraints(&mut self, handle: &RelationHandle) {
        for c in &handle.constraints {
            self.pending_constraints
                .entry(c.name().clone())
                .or_insert_with(|| c.clone());
        }
    }

    /// Create a relation, routed to the temp or persistent catalog by name.
    pub(crate) fn create_relation(
        &mut self,
        input: kyzo_model::program::InputRelationHandle,
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
    /// segment and generation slot after commit — every permanent retirement
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
    pub(crate) fn write_catalog_row(&mut self, handle: &RelationHandle) -> Result<()> {
        match handle.residency() {
            Residency::Temp => write_relation_row(&mut self.temp, handle),
            Residency::Stored => write_relation_row(&mut self.store, handle),
        }
    }

    /// Write one key/value into the store the relation lives in.
    pub(crate) fn put_routed(
        &mut self,
        residency: Residency,
        key: &[u8],
        val: &[u8],
    ) -> Result<()> {
        match residency {
            Residency::Temp => self.temp.put(key, val),
            Residency::Stored => self.store.put(key, val),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// The session view: what the query tier reads from a session
// ─────────────────────────────────────────────────────────────────────────

/// The read surface of one session, as the query tier consumes it: the
/// kernel transaction for stored relations, the scratch store for temp
/// relations, and name-routed catalog access over both. `Copy` by design —
/// it is two references.
pub struct SessionView<'a, T> {
    pub store: &'a T,
    pub temp: &'a TempTx,
}

impl<T> Clone for SessionView<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for SessionView<'_, T> {}

impl<'a, T: ReadTx> SessionView<'a, T> {
    /// Catalog lookup, routed by the relation-name namespace: `_`-prefixed
    /// names resolve in the session's temp catalog, everything else in the
    /// persistent catalog.
    pub(crate) fn handle(&self, name: &str) -> Result<RelationHandle> {
        if name.starts_with('_') {
            get_relation(self.temp, name)
        } else {
            get_relation(self.store, name)
        }
    }

    /// Scan every row of a relation through the routed reader, as-of
    /// `as_of` when time travel is requested.
    pub(crate) fn scan_all(&self, handle: &RelationHandle, as_of: Option<AsOf>) -> TupleIter<'a> {
        match (handle.residency(), as_of) {
            (Residency::Temp, None) => handle.scan_all(self.temp),
            (Residency::Temp, Some(vld)) => handle.skip_scan_all(self.temp, vld),
            (Residency::Stored, None) => handle.scan_all(self.store),
            (Residency::Stored, Some(vld)) => handle.skip_scan_all(self.store, vld),
        }
    }

    /// Prefix scan through the routed reader.
    pub(crate) fn scan_prefix(
        &self,
        handle: &RelationHandle,
        prefix: &Tuple,
        as_of: Option<AsOf>,
    ) -> TupleIter<'a> {
        match (handle.residency(), as_of) {
            (Residency::Temp, None) => handle.scan_prefix(self.temp, prefix),
            (Residency::Temp, Some(vld)) => handle.skip_scan_prefix(self.temp, prefix, vld),
            (Residency::Stored, None) => handle.scan_prefix(self.store, prefix),
            (Residency::Stored, Some(vld)) => handle.skip_scan_prefix(self.store, prefix, vld),
        }
    }
}

/// The magic tier's schema seam, served by the session view.
impl<T: ReadTx> StoredRelationSchemaSource for SessionView<'_, T> {
    fn stored_relation_schema(
        &self,
        name: &Symbol,
        _span: SourceSpan,
    ) -> Result<StoredRelationMetadata> {
        Ok(self.handle(&name.name)?.metadata)
    }
}

/// The fixed-rule payload's stored-input seam, served by the session view.
impl<T: ReadTx> StoredInputSource for SessionView<'_, T> {
    fn stored_arity(&self, name: &Symbol) -> Result<usize> {
        Ok(self.handle(&name.name)?.arity())
    }

    fn stored_scan_all<'b>(&'b self, name: &Symbol, as_of: Option<AsOf>) -> Result<TupleIter<'b>> {
        let handle = self.handle(&name.name)?;
        Ok(self.scan_all(&handle, as_of))
    }

    fn stored_scan_prefix<'b>(
        &'b self,
        name: &Symbol,
        prefix: &DataValue,
        as_of: Option<AsOf>,
    ) -> Result<TupleIter<'b>> {
        let handle = self.handle(&name.name)?;
        Ok(self.scan_prefix(&handle, &Tuple::from_vec(vec![prefix.clone()]), as_of))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::fjall::new_fjall_storage;
    use crate::store::sim::SimStorage;

    fn no_params() -> BTreeMap<String, DataValue> {
        BTreeMap::new()
    }

    /// Test-local composition: Store + fresh Catalog. Not the deleted fused
    /// public `Db::new(storage)` constructor — production callers use
    /// [`Engine::compose`].
    fn open_engine<S: Storage>(store: S) -> Engine<S> {
        Engine::compose(store, Catalog::new()).expect("compose engine")
    }

    /// Result rows as sorted `i64` vectors, for order-independent assertions.
    fn int_rows(nr: &NamedRows) -> Vec<Vec<i64>> {
        let mut out: Vec<Vec<i64>> = nr
            .rows()
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
        let db = open_engine(SimStorage::new(7));
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
        let db = open_engine(SimStorage::new(7));
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
        let refusal: &crate::exec::fixpoint::eval::LimitExceeded =
            err.downcast_ref().expect("typed budget refusal");
        assert_eq!(
            refusal.dimension,
            crate::exec::fixpoint::eval::BudgetDimension::InFlightDerivations,
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
        let db = open_engine(SimStorage::new(7));
        let err = db
            .run_script("?[a] := a in [1, 2, 3] :timeout 1e300", no_params())
            .expect_err("an unrepresentable timeout must refuse cleanly");
        assert!(
            matches!(
                err.downcast_ref::<EngineRefuse>(),
                Some(EngineRefuse::InvalidTimeout(_))
            ),
            "expected a typed InvalidTimeout refusal, got: {err}"
        );
    }

    /// Same reproduction via the `ScriptOptions.timeout_secs` Rust-API path
    /// (bypasses the parser's own `:timeout` handling entirely), and also
    /// covers infinity — both must be refused, never panic.
    #[test]
    fn huge_or_infinite_timeout_via_script_options_is_a_clean_error() {
        let db = open_engine(SimStorage::new(7));
        for bad in [1e300_f64, f64::INFINITY] {
            let opts = ScriptOptions {
                timeout_secs: Some(bad),
                ..Default::default()
            };
            let err = db
                .run_script_with("?[a] := a in [1, 2, 3]", no_params(), opts)
                .expect_err("an unrepresentable timeout must refuse cleanly, not panic");
            assert!(
                matches!(
                    err.downcast_ref::<EngineRefuse>(),
                    Some(EngineRefuse::InvalidTimeout(_))
                ),
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
        let db = open_engine(SimStorage::new(7));
        let err = db
            .run_script("?[x] := x = 2222222000*867076028303", no_params())
            .expect_err("an i64-overflowing literal product must refuse cleanly");
        // The op's `IntegerOverflow` is raised inside constant folding
        // (`Expr::eval` via `Expr::partial_eval`), which wraps every op
        // error as `EvalRaisedError` (its message, not its type, survives
        // — in the struct's own help string, not its fixed `Display`) —
        // the same wrapping every other op error gets.
        let wrapped = match err.downcast_ref::<kyzo_model::program::expr::EvalRaisedError>() {
            Some(w) => w,
            None => {
                assert!(
                    false,
                    "expected an EvalRaisedError wrapping the overflow, got: {err}"
                );
                return;
            }
        };
        assert!(
            wrapped.1.contains("integer overflow"),
            "expected an integer-overflow message, got: {}",
            wrapped.1
        );
    }

    /// THE FIRST END-TO-END QUERY: create → insert → recursive query with a
    /// join → exact rows, all through the public `Db::run_script` over a real
    /// backend. Parameterized so the same script runs on fjall and mem.
    fn create_insert_recursive_join<S: Storage>(db: Engine<S>) {
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
        let db = open_engine(new_fjall_storage(dir.path()).unwrap());
        create_insert_recursive_join(db);
    }

    #[test]
    fn first_end_to_end_query_over_mem() {
        let db = open_engine(SimStorage::new(7));
        create_insert_recursive_join(db);
    }

    /// Two sessions racing read-modify-write on one counter row over the real
    /// concurrent backend: each increment reads the counter (putting it in the
    /// conflict set) then writes it back, so colliding commits force one to
    /// take a typed `ConflictError` and replay. If the retry loop worked and no
    /// update was lost, the final value equals the total number of increments.
    #[test]
    fn retry_under_contention_loses_no_update() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_engine(new_fjall_storage(dir.path()).unwrap());
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
        fn drive<S: Storage>(db: Engine<S>) {
            db.run_script("?[k, v] <- [[1, 'first']] :create t {k => v}", no_params())
                .expect("create");
            db.run_script("?[k] <- [[1]] :rm t {k}", no_params())
                .expect("rm");
            let gone = db
                .run_script("?[k, v] := *t[k, v]", no_params())
                .expect("read");
            assert!(
                gone.rows().is_empty(),
                "retracted fact must be absent: {gone:?}"
            );
            db.run_script("?[k, v] <- [[1, 'second']] :put t {k => v}", no_params())
                .expect("reinsert");
            let back = db
                .run_script("?[k, v] := *t[k, v]", no_params())
                .expect("read");
            assert_eq!(back.rows().len(), 1, "reinserted fact must be present");
            assert_eq!(back.rows()[0][1], DataValue::from("second"));
            // And once more: the second retraction must also govern.
            db.run_script("?[k] <- [[1]] :rm t {k}", no_params())
                .expect("rm again");
            let gone = db
                .run_script("?[k, v] := *t[k, v]", no_params())
                .expect("read");
            assert!(gone.rows().is_empty(), "re-retracted fact must be absent");
        }
        let dir = tempfile::tempdir().unwrap();
        drive(open_engine(new_fjall_storage(dir.path()).unwrap()));
        drive(open_engine(crate::store::sim::SimStorage::new(7)));
    }

    fn current<S: Storage>(db: &Engine<S>) -> i64 {
        let out = db
            .run_script("?[v] := *ctr[k, v]", no_params())
            .expect("read counter");
        out.rows()[0][0].get_int().expect("int")
    }

    /// A deterministic derived-tuple ceiling refuses a query that would derive
    /// more, through the public API — a typed refusal, reproducibly, with no
    /// wall-clock dependence.
    #[test]
    fn budget_refusal_is_deterministic_and_typed() {
        let db = open_engine(SimStorage::new(5));
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
        assert_eq!(ok.rows().len(), 12);
    }

    /// Story #68 / issue #1: a value-generating recursion with NO fixpoint
    /// (`f[x] := f[y], x = y + 1` — every epoch derives exactly one new,
    /// never-before-seen `x`) used to be bounded ONLY by the epoch ceiling;
    /// measured directly on this tree, driving it to the full
    /// 1,000,000-epoch default takes ~30s of CPU per request — cheap enough
    /// to hammer a live server with concurrently, expensive enough per
    /// request to be a real denial-of-service surface. A small EXPLICIT
    /// derived-tuple ceiling must refuse it instantly, naming the
    /// `derived tuples` dimension (not `epochs`) — proving the NEW ceiling,
    /// not the pre-existing one, is what catches it.
    #[test]
    fn runaway_value_generating_recursion_refuses_under_explicit_ceiling() {
        let db = open_engine(SimStorage::new(11));
        let opts = ScriptOptions {
            derived_tuple_ceiling: Some(10),
            ..Default::default()
        };
        let err = db
            .run_script_with(
                "
                f[x] := x = 1
                f[x] := f[y], x = y + 1
                ?[x] := f[x]
                ",
                no_params(),
                opts,
            )
            .expect_err("a recursion with no fixpoint must refuse, never hang");
        let refusal: &crate::exec::fixpoint::eval::LimitExceeded = err
            .downcast_ref()
            .expect("typed budget refusal, not a panic or a hang");
        assert_eq!(
            refusal.dimension,
            crate::exec::fixpoint::eval::BudgetDimension::DerivedTuples,
            "must name the derived-tuple dimension specifically"
        );
        assert_eq!(refusal.ceiling, 10);
    }

    /// A WIDENING value-generating recursion — every epoch's join fans out
    /// (`x = y*2` AND `x = y*2+1` from the same `y`, a binary tree over the
    /// unbounded positive integers with no fixpoint) — under COMPLETELY
    /// DEFAULT [`ScriptOptions`] (no explicit ceiling of any kind). This is
    /// the class [`DEFAULT_DERIVED_TUPLE_CEILING`] exists for: an epoch
    /// ceiling alone bounds ITERATION COUNT, not per-iteration volume, so it
    /// cannot stop a rule whose output doubles every epoch from exhausting
    /// memory in a couple dozen epochs — far short of
    /// `DEFAULT_EPOCH_CEILING`'s 1,000,000. Before this fix
    /// `derived_tuple_ceiling` defaulted to `None` (truly unbounded); it must
    /// now refuse on the `derived tuples` dimension, naming the default
    /// ceiling exactly. (Measured: ~30s to the typed refusal at ~50M rows
    /// admitted, peak RSS ~2.4GB — bounded and finite, never a silent hang
    /// or an unbounded climb; this is the one deliberately expensive test in
    /// this file, verifying the actual compiled-in default end to end.)
    #[test]
    fn widening_value_generating_recursion_refuses_under_default_budget() {
        let db = open_engine(SimStorage::new(12));
        let err = db
            .run_script(
                "
                f[x] := x = 1
                f[x] := f[y], x = y * 2
                f[x] := f[y], x = y * 2 + 1
                ?[x] := f[x]
                ",
                no_params(),
            )
            .expect_err("the DEFAULT budget alone must refuse a fixpoint-less, widening recursion");
        let refusal: &crate::exec::fixpoint::eval::LimitExceeded = err
            .downcast_ref()
            .expect("typed budget refusal, not a panic or a hang");
        assert_eq!(
            refusal.dimension,
            crate::exec::fixpoint::eval::BudgetDimension::DerivedTuples,
            "the default derived-tuple ceiling, not the pre-existing epoch ceiling, must be \
             what catches this — a fall-through to Epochs would mean the fix did nothing for \
             a widening recursion, which can exhaust memory in far fewer than 1,000,000 epochs"
        );
        assert_eq!(refusal.ceiling, DEFAULT_DERIVED_TUPLE_CEILING);
    }

    /// Raising `derived_tuple_ceiling` through [`ScriptOptions`] lets a
    /// bigger — but genuinely terminating — query run. A 1000-node path's
    /// full transitive closure admits `999 + 998 + ... + 1 = 499_500` pairs
    /// into `path`, and the entry rule `?[a, b] := path[a, b]` admits the
    /// same 499_500 again into the output store — `DerivedTuples` sums
    /// admissions across every store for the whole query (`eval.rs`), so the
    /// true spend is ~999_000 (confirmed empirically: an explicit ceiling of
    /// 999_000 completes, 900_000 still refuses). Two EXPLICIT ceilings
    /// (never the compiled-in default, so this test is independent of its
    /// exact value) bracket that true spend: a low one must refuse, a
    /// higher one must admit the whole, finite, correct answer — this is not
    /// runaway, it is a normal terminating recursion whose answer is simply
    /// large, and the override path must not turn any ceiling into a hard
    /// cap on legitimate work.
    #[test]
    fn raising_derived_tuple_ceiling_admits_a_larger_terminating_query() {
        let db = open_engine(SimStorage::new(13));
        let mut edges = String::from("?[a, b] <- [");
        for i in 0..999 {
            edges.push_str(&format!("[{i}, {}],", i + 1));
        }
        edges.push_str("] :create edge {a, b}");
        db.run_script(&edges, no_params()).expect("create edges");

        let q = "
            path[a, b] := *edge[a, b]
            path[a, b] := *edge[a, c], path[c, b]
            ?[a, b] := path[a, b]
            ";

        // A low explicit ceiling (well under the true ~999_000 spend)
        // refuses — a single epoch's join here materializes rows faster
        // than the epoch barrier, so the mid-epoch `InFlightDerivations`
        // guard (checked every `INTERRUPT_STRIDE` derivations, see
        // `eval::InterruptTicker`) trips first; either way it is the SAME
        // armed derived-tuple ceiling that stops it, never a silent hang.
        let low_opts = ScriptOptions {
            derived_tuple_ceiling: Some(200_000),
            ..Default::default()
        };
        let err = db
            .run_script_with(q, no_params(), low_opts)
            .expect_err("~999_000 true spend must exceed a 200_000 ceiling");
        let refusal: &crate::exec::fixpoint::eval::LimitExceeded =
            err.downcast_ref().expect("typed budget refusal");
        assert!(
            matches!(
                refusal.dimension,
                crate::exec::fixpoint::eval::BudgetDimension::DerivedTuples
                    | crate::exec::fixpoint::eval::BudgetDimension::InFlightDerivations
            ),
            "expected a derived-tuple-ceiling refusal, got {:?}",
            refusal.dimension
        );
        assert_eq!(refusal.ceiling, 200_000);

        // Raising the ceiling admits the whole (finite, correct) answer.
        // True total spend across `path` + the entry store is ~999_000
        // (measured); 1_100_000 gives real headroom.
        let high_opts = ScriptOptions {
            derived_tuple_ceiling: Some(1_100_000),
            ..Default::default()
        };
        let ok = db
            .run_script_with(q, no_params(), high_opts)
            .expect("a raised ceiling must let the larger terminating query complete");
        assert_eq!(ok.rows().len(), 499_500);
    }
}

#[cfg(test)]
mod db_battery {
    //! Absorbed from runtime/db_battery.rs (story #350 T2): e2e, contention,
    //! determinism, and F2 refusal pin. Trigger-cache kill lives in admit;
    //! callback exactly-once arms live in observe.

    use std::collections::BTreeMap;

    use crate::data::json::NamedRows;
    use crate::session::catalog::Catalog;
    use crate::session::db::{Engine, ScriptOptions, SessionTx};
    use crate::store::Storage;
    use crate::store::fjall::new_fjall_storage;
    use crate::store::sim::SimStorage;
    use kyzo_model::value::DataValue;

    fn no_params() -> BTreeMap<String, DataValue> {
        BTreeMap::new()
    }

    fn open_engine<S: Storage>(store: S) -> Engine<S> {
        Engine::compose(store, Catalog::new()).expect("compose engine")
    }

    fn int_rows(nr: &NamedRows) -> Vec<Vec<i64>> {
        let mut out: Vec<Vec<i64>> = nr
            .rows()
            .iter()
            .map(|r| r.iter().map(|v| v.get_int().expect("int")).collect())
            .collect();
        out.sort();
        out
    }

    fn raw_int_rows(nr: &NamedRows) -> Vec<Vec<i64>> {
        nr.rows()
            .iter()
            .map(|r| r.iter().map(|v| v.get_int().expect("int")).collect())
            .collect()
    }

    /// Reviewer's own end-to-end scenario over fjall: schema with keyed relation,
    /// multi-script inserts, aggregation, :order/:limit, :update, :insert
    /// conflict, :ensure, :rm — the stored.rs arms the author's tests never touch.
    #[test]
    fn rs3_independent_e2e_scenario() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_engine(new_fjall_storage(dir.path()).unwrap());

        db.run_script(
            "?[a, b] <- [[1, 10], [2, 20]] :create sal {a => b}",
            no_params(),
        )
        .expect("create");
        db.run_script("?[a, b] <- [[3, 30], [4, 20]] :put sal {a, b}", no_params())
            .expect("second put");

        // Aggregation through the public API.
        let agg = db
            .run_script("?[sum(b)] := *sal[_, b]", no_params())
            .expect("aggregation");
        assert_eq!(int_rows(&agg), vec![vec![80]]);

        // :order desc by value, tie broken asc by key, :limit 2.
        let top = db
            .run_script("?[a, b] := *sal[a, b] :order -b, a :limit 2", no_params())
            .expect("order+limit");
        assert_eq!(raw_int_rows(&top), vec![vec![3, 30], vec![2, 20]]);

        // :update rewrites the dependent column of an existing key.
        db.run_script("?[a, b] <- [[1, 11]] :update sal {a, b}", no_params())
            .expect("update");
        let after_update = db
            .run_script("?[b] := *sal[1, b]", no_params())
            .expect("read back");
        assert_eq!(int_rows(&after_update), vec![vec![11]]);

        // :insert on an existing key is a typed refusal.
        let err = db
            .run_script("?[a, b] <- [[1, 99]] :insert sal {a, b}", no_params())
            .expect_err(":insert must refuse an existing key");
        assert!(
            format!("{err:?}").contains("exists"),
            "expected key-exists refusal, got {err:?}"
        );

        // :ensure passes on a matching row, refuses a mismatch.
        db.run_script("?[a, b] <- [[1, 11]] :ensure sal {a, b}", no_params())
            .expect(":ensure matching row");
        db.run_script("?[a, b] <- [[1, 12]] :ensure sal {a, b}", no_params())
            .expect_err(":ensure must refuse a mismatched value");

        // :rm removes by key; survivors are exactly the rest.
        db.run_script("?[a] <- [[1], [4]] :rm sal {a}", no_params())
            .expect("rm");
        let rest = db
            .run_script("?[a, b] := *sal[a, b]", no_params())
            .expect("scan");
        assert_eq!(int_rows(&rest), vec![vec![2, 20], vec![3, 30]]);

        // :returning on a put reports the mutated rows.
        let ret = db
            .run_script(
                "?[a, b] <- [[7, 70]] :put sal {a, b} :returning",
                no_params(),
            )
            .expect("put returning");
        assert_eq!(int_rows(&ret), vec![vec![7, 70]]);
    }

    /// Reviewer's own contention shape (3 writers, distinct from the author's 2).
    #[test]
    fn rs3_three_writer_contention_loses_no_update() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_engine(new_fjall_storage(dir.path()).unwrap());
        db.run_script("?[k, v] <- [[0, 0]] :create ctr {k => v}", no_params())
            .expect("create counter");

        const PER_THREAD: i64 = 10;
        std::thread::scope(|scope| {
            for _ in 0..3 {
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
        let out = db
            .run_script("?[v] := *ctr[0, v]", no_params())
            .expect("read");
        assert_eq!(int_rows(&out), vec![vec![30]]);
    }

    /// Determinism: the same scenario on two fresh databases — and across the
    /// fjall and sim backends — returns byte-identical rows in identical order;
    /// a budget refusal renders identically on repeated runs.
    #[test]
    fn rs3_determinism_across_backends_and_repeats() {
        fn scenario<S: Storage>(db: &Engine<S>) -> (Vec<Vec<i64>>, Vec<Vec<i64>>) {
            db.run_script(
                "?[a, b] <- [[1, 2], [2, 3], [3, 4], [4, 2], [5, 6]] :create edge {a, b}",
                no_params(),
            )
            .expect("create");
            let q = "
                path[a, b] := *edge[a, b]
                path[a, b] := *edge[a, c], path[c, b]
                ?[a, b] := path[a, b]
            ";
            let first = raw_int_rows(&db.run_script(q, no_params()).expect("closure"));
            let second = raw_int_rows(&db.run_script(q, no_params()).expect("closure again"));
            (first, second)
        }

        let dir1 = tempfile::tempdir().unwrap();
        let db1 = open_engine(new_fjall_storage(dir1.path()).unwrap());
        let dir2 = tempfile::tempdir().unwrap();
        let db2 = open_engine(new_fjall_storage(dir2.path()).unwrap());
        let db3 = open_engine(SimStorage::new(99));

        let (a1, a2) = scenario(&db1);
        let (b1, _) = scenario(&db2);
        let (c1, _) = scenario(&db3);
        assert_eq!(a1, a2, "same db, repeated run: identical rows in order");
        assert_eq!(a1, b1, "fresh fjall dbs: identical rows in order");
        assert_eq!(a1, c1, "fjall vs sim: identical rows in order");

        // Budget refusal is reproducible, including its rendered content.
        let refusal = |db: &Engine<SimStorage>| -> String {
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
                .expect_err("must refuse");
            format!("{err:?}")
        };
        let r1 = refusal(&db3);
        let r2 = refusal(&db3);
        let r3 = refusal(&db3);
        assert_eq!(r1, r2);
        assert_eq!(r2, r3);
    }

    /// F2 FIXED (was: silently dropped): a mutation targeting a `_`-prefixed
    /// (temp) relation would be routed down the read-only path by
    /// `needs_write_lock() == None` and its `store_relation` silently ignored.
    /// It is now a typed, spanned refusal (`TempRelationNotReachableError`)
    /// until multi-script sessions make temp relations observable. Weakening
    /// the refusal back to the silent drop makes this test fail on the
    /// `unwrap_err`.
    #[test]
    fn rs3_temp_relation_mutation_is_a_typed_refusal() {
        let db = open_engine(SimStorage::new(23));
        let err = db
            .run_script("?[a] <- [[1]] :create _scratch {a}", no_params())
            .unwrap_err();
        assert!(
            err.to_string().contains("cannot be stored to yet"),
            "expected the typed temp-relation refusal, got: {err}"
        );
        // The refusal really was a refusal: nothing half-created.
        db.run_script("?[a] := *_scratch[a]", no_params())
            .expect_err("the temp relation must not exist after the refusal");
    }

    /// #375 T1 nasty: PRODUCTION [`SessionTx::commit_write`] twice with the
    /// same client operation identity — exactly one SweepDoor CommitOrdinal.
    #[test]
    fn operation_key_commit_write_dedupes_same_operation_identity() {
        let db = open_engine(SimStorage::new(0x3750_00db));
        let opts = ScriptOptions {
            client_operation_id: Some(b"db-op-key-dedupe".to_vec()),
            sweep: Some(db.sweep.clone()),
            ..ScriptOptions::default()
        };
        SessionTx::new_write(db.store.write_tx().expect("tx1"), opts.clone())
            .commit_write()
            .expect("first commit_write");
        SessionTx::new_write(db.store.write_tx().expect("tx2"), opts)
            .commit_write()
            .expect("retry commit_write");
        let commits = db
            .sweep
            .with_mut(|door, _, _| door.highest_commit_ordinal().get());
        assert_eq!(
            commits, 1,
            "two production commit_write calls with the same OperationKey \
             must mint exactly one committed effect"
        );
    }

    /// #374 T10 nasty: acknowledge a live KyzoScript write, inject a
    /// power-cut that drops non-fsync'd data, reopen, and assert the
    /// acknowledged write survives. RED on a bare `WriteTx::commit()` ack
    /// path (buffer-tier only); GREEN only when `commit_write` passes the
    /// NativeFsyncProof StableCommitCap barrier (`commit_durable`).
    #[test]
    fn live_write_ack_survives_power_cut_via_stable_commit_cap() {
        let store = SimStorage::new(37_410);
        let db = open_engine(store);
        db.run_script("?[x] <- [[42]] :create ack_survive {x}", no_params())
            .expect("live KyzoScript write must acknowledge");
        // Power cut after ack: only the fsynced prefix survives.
        let after_cut = db.store.sim_powercut();
        let reopened = open_engine(after_cut);
        let rows = reopened
            .run_script("?[x] := *ack_survive[x]", no_params())
            .expect("acked write must be query-visible after power cut");
        assert_eq!(
            int_rows(&rows),
            vec![vec![42]],
            "acknowledged live write must survive SimStorage::sim_powercut \
             (StableCommitCap NativeFsyncProof barrier)"
        );
    }

    /// #374 T10: barrier-on-ack — injected fsync failure must refuse the
    /// acknowledgement (never production Committed / silent volatile ack).
    #[test]
    fn live_write_ack_refuses_when_fsync_barrier_fails() {
        let store = SimStorage::with_faults(
            37_411,
            crate::store::sim::FaultConfig {
                sync_fail_ppm: 1_000_000,
                ..Default::default()
            },
        );
        let db = open_engine(store);
        let err = db
            .run_script("?[x] <- [[7]] :create ack_fsync_fail {x}", no_params())
            .expect_err("fsync-barrier failure must refuse live ack");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("fsync") || msg.contains("sync") || msg.contains("Io"),
            "typed durability shortfall on ack, got: {msg}"
        );
        // Nothing half-acked at the durable tier.
        let after_cut = db.store.sim_powercut();
        let reopened = open_engine(after_cut);
        reopened
            .run_script("?[x] := *ack_fsync_fail[x]", no_params())
            .expect_err("refused ack must not leave a durable relation");
    }
}
