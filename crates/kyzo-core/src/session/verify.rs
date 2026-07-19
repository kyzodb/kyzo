/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! `::verify` (story #80): the self-adversary primitive. Run one query
//! through the production evaluator AND the sealed reference oracle
//! (`query/laws.rs`) against ONE shared SSI snapshot, and report MATCH, a
//! reproducible MISMATCH, or a typed refusal — "no competing database ships
//! its own adversary."
//!
//! ## Scope of this cut, stated plainly
//!
//! The oracle (`query/laws.rs::Program`) names relations and variables as
//! content-eq [`laws::Name`] (`Arc<str>`). A live, parsed KyzoScript
//! program's names are runtime [`Symbol`]s; translation mints each name
//! through [`laws::Name::owned`] — a typed proof bridge with no
//! process-global leak-intern table (P115).
//!
//! The translator covers plain relational Datalog: rule/relation
//! applications (positive and negated), recursion, per-head-position
//! aggregation, and point-in-time historical reads (`@ <coordinate>`,
//! [`crate::data::program::ValidityClause::At`]). It REFUSES, typed, rather
//! than silently mistranslating: fixed-rule applications (no generic bridge
//! from `Arc<dyn FixedRule>` to the oracle's
//! `fn(&[BTreeSet<Tuple>]) -> BTreeSet<Tuple>` shape), predicate/unification
//! atoms (arbitrary `Expr` evaluation is outside the oracle's plain
//! `Term::Var`/`Term::Const` model), index-search atoms,
//! interval-derivation/diff reads (`@spans`/`@delta`/`@delta_sys` —
//! these bind an EXTRA column beyond the relation's own arity, a distinct
//! translator shape from the point-in-time `@` case just landed; named
//! follow-on, not attempted here), and `:order`/`:limit`/`:offset`/mutation
//! queries (this cut compares full, unordered read-only answer sets). Every
//! refusal is a [`VerifyOutcome::Unsupported`], never a silent pass and
//! never a panic.
//!
//! **A finding along the way, named rather than routed around:** a variable
//! appearing ONLY inside a negated literal (including the `_` wildcard
//! sugar) is refused as `Unsafe` by the oracle's `check_safety` even on
//! programs the production compiler's OWN safety check accepts (e.g. `not
//! *hist[k, _ @ t]` where `_` never appears elsewhere) — a real, narrower
//! safety-notion gap between the two, distinct from the `@spans`/`@delta`
//! boundary above. Every historical-read test in this module's `tests`
//! module below binds every negated-literal variable positively first
//! (unambiguously safe under either notion) to isolate the case this cut
//! actually proves; the wildcard-in-negation gap is not otherwise routed
//! around and is left for the corpus/refusal-fence work to characterize
//! fully.
//!
//! ## The snapshot adapter
//!
//! One `ReadTx` is opened once and used for BOTH evaluators — the
//! production path via [`Db::compile_and_eval`] and the oracle's EDB feed
//! via [`StoredWithValidityRA::iter_batched`] for current-state relations,
//! or [`crate::query::ra::temporal::decode_raw_version`]'s raw multi-version
//! scan for historical ones — the exact scan/decode primitives the
//! production compiler itself uses for a stored-relation atom, `AsOf`
//! resolution included — so "byte-identical state" is structural, not a
//! hope: no second, independent scan or decode of the same relation ever
//! runs. Consequence stated plainly: `::verify`'s temporal independence
//! lives in the EVALUATION (the oracle's naive fixpoint vs. the production
//! compiler's), not in the raw-version decode — a bug in
//! `decode_raw_version` itself would be shared by both sides and could
//! escape this check, exactly as a `range_skip_scan_tuple` bug could escape
//! the current-state check above it.

use std::collections::{BTreeMap, BTreeSet};

use miette::{Diagnostic, Result};
use thiserror::Error;

use crate::data::program::{
    InputProgram, NormalFormAtom, NormalFormInlineRule, NormalFormRulesOrFixed,
};
use kyzo_model::program::symbol::Symbol;
use kyzo_model::value::Tuple;
use kyzo_model::value::{AsOf, ValidityTs};
use crate::fixed_rule::CancelFlag;
use crate::parse::{Script, parse_script};
use crate::query::laws;
use crate::query::normalize::{SessionNormalizer, SessionView};
use crate::query::ra::stored::StoredWithValidityRA;
use crate::query::temp_store::TupleInIter;
use crate::session::catalog::get_relation;
use crate::session::current_validity;
use crate::session::db::{Db, ScriptOptions, SessionTx};
use crate::storage::{ReadTx, Storage};

/// The outcome of one `::verify` run. Never a bare bool: a MATCH, a
/// reproducible MISMATCH (both answer sets, for a filed engine issue), or a
/// named reason neither evaluator produced a comparable answer.
#[derive(Debug, Clone)]
pub enum VerifyOutcome {
    /// Production and the oracle agree, set-for-set.
    Match { row_count: usize },
    /// A real disagreement: the reproducible bug report. `program` is the
    /// typed input that Display-renders to the script re-run to reproduce it.
    Mismatch {
        program: MismatchProgram,
        production: BTreeSet<Tuple>,
        oracle: BTreeSet<Tuple>,
    },
    /// The query uses a construct this cut's translator does not carry to
    /// the oracle (see the module docs' scope section). Named, not silent.
    /// Holds [`VerifyUnsupported`] typed; rendered only in [`Self::into_named_rows`].
    Unsupported { reason: VerifyUnsupported },
    /// The oracle itself refused the translated program (unsafe or
    /// unstratifiable) — a genuine finding about the QUERY, not a verify
    /// harness bug, and not evidence of an engine defect since the
    /// production compiler independently refuses the same programs.
    /// Holds [`OracleRefusal`] typed; rendered only in [`Self::into_named_rows`].
    OracleRefused { reason: OracleRefusal },
}

/// Typed program carried by [`VerifyOutcome::Mismatch`].
/// Wraps [`InputProgram`]; formatting for the product row lives only in
/// [`VerifyOutcome::into_named_rows`].
#[derive(Debug, Clone)]
pub struct MismatchProgram(pub(crate) InputProgram);

impl std::fmt::Display for MismatchProgram {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

/// Typed oracle refusal carried by [`VerifyOutcome::OracleRefused`].
/// Wraps [`laws::Rejection`]; formatting for the product row lives only in
/// [`VerifyOutcome::into_named_rows`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OracleRefusal(pub(crate) laws::Rejection);

impl From<laws::Rejection> for OracleRefusal {
    fn from(rejection: laws::Rejection) -> Self {
        OracleRefusal(rejection)
    }
}

/// Named reason [`VerifyOutcome::Unsupported`] carries — never a bare
/// `miette!` string. Display is for the product-row edge only
/// ([`VerifyOutcome::into_named_rows`]), never stored as a `String` on the outcome.
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
         ::verify — it compares full, unordered answer sets"
    )]
    #[diagnostic(code(verify::order_limit_offset))]
    OrderLimitOffset,
    #[error(transparent)]
    #[diagnostic(transparent)]
    Translate(TranslateUnsupported),
}

/// A construct this cut's translator refuses to carry to the oracle —
/// caught at the top level and turned into [`VerifyOutcome::Unsupported`],
/// never a hard error (an unsupported query is a normal, named outcome).
#[derive(Debug, Clone, PartialEq, Eq, Error, Diagnostic)]
pub enum TranslateUnsupported {
    #[error(
        "relation atom '{name}' is an interval-derivation (@spans) or diff \
         (@delta/@delta_sys) read: these bind an extra column beyond the \
         relation's own arity, a distinct translator shape from the \
         point-in-time @ read just landed — not yet translated"
    )]
    #[diagnostic(code(verify::translate::interval_derivation))]
    IntervalDerivation { name: Symbol },
    #[error(
        "predicate (filter expression) atoms are not translated — the oracle's Term \
         model has no arbitrary-expression evaluation"
    )]
    #[diagnostic(code(verify::translate::predicate))]
    Predicate,
    #[error(
        "unification ('=' / 'in') atoms are not translated — the oracle's Term model \
         has no arbitrary-expression evaluation"
    )]
    #[diagnostic(code(verify::translate::unification))]
    Unification,
    #[error("index-search atoms (~rel:idx{{...}}) have no oracle-model equivalent")]
    #[diagnostic(code(verify::translate::search))]
    Search,
    #[error(
        "'{rel}' is defined by a fixed-rule application ('{fixed}'); ::verify has no generic \
         bridge from a live fixed-rule implementation to the oracle's plain-function \
         model"
    )]
    #[diagnostic(code(verify::translate::fixed_rule))]
    FixedRule { rel: Symbol, fixed: Symbol },
}

impl From<VerifyUnsupported> for VerifyOutcome {
    fn from(reason: VerifyUnsupported) -> Self {
        VerifyOutcome::Unsupported { reason }
    }
}

/// Named refusal when `:timeout` is not a usable finite duration.
#[derive(Debug, Clone, PartialEq, Error, Diagnostic)]
#[error("timeout {secs} is not a usable duration")]
#[diagnostic(code(verify::unusable_timeout))]
struct UnusableTimeout {
    secs: f64,
}

impl VerifyOutcome {
    /// Render as the `::verify { ... }` script directive's result table
    /// (`SysOp::Verify`'s dispatcher, `runtime/db.rs::run_sys_op`) — the
    /// product-surface rendering of the same outcome
    /// [`Db::verify_script`]/[`Db::verify_input_program`] return as a typed
    /// Rust value. One row, `["status", "summary", "detail"]`.
    pub(crate) fn into_named_rows(self) -> crate::fixed_rule::NamedRows {
        let (status, summary, detail) = match self {
            VerifyOutcome::Match { row_count } => {
                ("match", format!("{row_count} row(s) agree"), String::new())
            }
            VerifyOutcome::Mismatch {
                program,
                production,
                oracle,
            } => (
                "mismatch",
                format!(
                    "production {} row(s) vs oracle {} row(s)",
                    production.len(),
                    oracle.len()
                ),
                format!("program:\n{program}\nproduction: {production:?}\noracle: {oracle:?}"),
            ),
            VerifyOutcome::Unsupported { reason } => {
                ("unsupported", reason.to_string(), String::new())
            }
            VerifyOutcome::OracleRefused { reason } => {
                ("oracle_refused", format!("{:?}", reason.0), String::new())
            }
        };
        // Three headers, one width-3 row — by construction.
        crate::fixed_rule::NamedRows::verify_status_row(status, summary, detail)
    }
}

/// Mint an oracle [`laws::Name`] from a runtime symbol — the sole
/// verify→oracle name door (P115). Owned `Arc<str>`; no leak-intern set.
fn oracle_name(sym: &Symbol) -> laws::Name {
    laws::Name::owned(sym.name.as_str())
}

/// The oracle-bound program plus the EDB relation names it reads (still to
/// be scanned) and the entry relation's oracle name.
struct Translated {
    program: laws::Program,
    edb_names: BTreeSet<laws::Rel>,
    /// Relations read with an explicit `@` (point-in-time) validity clause
    /// SOMEWHERE in the program — every read of these (clause-carrying or
    /// not) resolves through `laws::Program::histories`, never `facts`
    /// (`check_wellformed`'s XOR). `@spans`/`@delta` clauses are not in this
    /// set — see [`translate_atom`]'s `Spans`/`Delta` arm.
    historical_names: BTreeSet<laws::Rel>,
    entry_rel: laws::Rel,
}

fn translate_term(sym: &Symbol) -> laws::Term {
    laws::Term::var(oracle_name(sym))
}

/// The real bitemporal `AsOf` (`ValidityTs::from_raw(_)`, descending) into
/// the oracle's own plain-ascending `laws::AsOf` — the exact correspondence
/// `laws.rs`'s own module doc states and
/// `asof_mirror_matches_bitemporal_kernel_on_a_shared_fixture` proves.
fn to_oracle_asof(real: kyzo_model::value::AsOf) -> laws::AsOf {
    laws::AsOf {
        valid: real.valid().raw(),
        sys: real.sys().raw(),
    }
}

fn translate_atom(
    atom: &NormalFormAtom,
    edb_names: &mut BTreeSet<laws::Rel>,
    historical_names: &mut BTreeSet<laws::Rel>,
) -> std::result::Result<laws::Literal, TranslateUnsupported> {
    use crate::data::program::ValidityClause;

    match atom {
        NormalFormAtom::Rule(a) => Ok(laws::Literal::pos(
            oracle_name(&a.name),
            a.args.iter().map(translate_term).collect(),
        )),
        NormalFormAtom::NegatedRule(a) => Ok(laws::Literal::neg(
            oracle_name(&a.name),
            a.args.iter().map(translate_term).collect(),
        )),
        NormalFormAtom::Relation(a) => {
            let rel = oracle_name(&a.name);
            let args: Vec<laws::Term> = a.args.iter().map(translate_term).collect();
            match &a.validity {
                None => {
                    edb_names.insert(rel.clone());
                    Ok(laws::Literal::pos(rel, args))
                }
                Some(ValidityClause::At(as_of)) => {
                    historical_names.insert(rel.clone());
                    Ok(laws::Literal::pos_at(rel, args, to_oracle_asof(*as_of)))
                }
                Some(ValidityClause::Spans { .. } | ValidityClause::Delta { .. }) => {
                    Err(TranslateUnsupported::IntervalDerivation {
                        name: a.name.clone(),
                    })
                }
            }
        }
        NormalFormAtom::NegatedRelation(a) => {
            let rel = oracle_name(&a.name);
            let args: Vec<laws::Term> = a.args.iter().map(translate_term).collect();
            match &a.validity {
                None => {
                    edb_names.insert(rel.clone());
                    Ok(laws::Literal::neg(rel, args))
                }
                Some(ValidityClause::At(as_of)) => {
                    historical_names.insert(rel.clone());
                    Ok(laws::Literal::neg_at(rel, args, to_oracle_asof(*as_of)))
                }
                Some(ValidityClause::Spans { .. } | ValidityClause::Delta { .. }) => {
                    Err(TranslateUnsupported::IntervalDerivation {
                        name: a.name.clone(),
                    })
                }
            }
        }
        NormalFormAtom::Predicate(_) => Err(TranslateUnsupported::Predicate),
        NormalFormAtom::Unification(_) => Err(TranslateUnsupported::Unification),
        NormalFormAtom::Search(_) => Err(TranslateUnsupported::Search),
    }
}

fn translate_inline_rule(
    head_rel: laws::Rel,
    rule: &NormalFormInlineRule,
    edb_names: &mut BTreeSet<laws::Rel>,
    historical_names: &mut BTreeSet<laws::Rel>,
) -> std::result::Result<laws::Rule, TranslateUnsupported> {
    let head_args: Vec<laws::Term> = rule.head.iter().map(translate_term).collect();
    let body = rule
        .body
        .iter()
        .map(|a| translate_atom(a, edb_names, historical_names))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(laws::Rule::aggregated(
        head_rel,
        head_args,
        rule.aggr.clone(),
        body,
    ))
}

fn translate_def(
    name: &Symbol,
    def: &NormalFormRulesOrFixed,
    rules: &mut Vec<laws::Rule>,
    edb_names: &mut BTreeSet<laws::Rel>,
    historical_names: &mut BTreeSet<laws::Rel>,
) -> std::result::Result<(), TranslateUnsupported> {
    match def {
        NormalFormRulesOrFixed::Fixed { fixed } => Err(TranslateUnsupported::FixedRule {
            rel: name.clone(),
            fixed: fixed.fixed_handle.name.clone(),
        }),
        NormalFormRulesOrFixed::Rules { rules: inline } => {
            let name_rel = oracle_name(name);
            for r in inline {
                rules.push(translate_inline_rule(
                    name_rel.clone(),
                    r,
                    edb_names,
                    historical_names,
                )?);
            }
            Ok(())
        }
    }
}

/// Translate a normalized program into the oracle's model, refusing (typed,
/// not silently) any construct outside this cut's scope. Every rule
/// definition in the program is translated regardless of reachability from
/// the entry — harmless: an unused oracle rule never affects the entry's
/// fixpoint, and `laws.rs` infers IDB-ness from which relations are some
/// rule's head, not from a separately declared set.
fn translate(
    program: &crate::data::program::NormalFormProgram,
) -> std::result::Result<Translated, TranslateUnsupported> {
    let mut rules = Vec::new();
    let mut edb_names = BTreeSet::new();
    let mut historical_names = BTreeSet::new();

    for (name, def) in program.rules() {
        translate_def(name, def, &mut rules, &mut edb_names, &mut historical_names)?;
    }
    translate_def(
        program.entry_name(),
        program.entry(),
        &mut rules,
        &mut edb_names,
        &mut historical_names,
    )?;
    let entry_rel = oracle_name(program.entry_name());

    // The XOR: a relation read with ANY `@` clause anywhere resolves
    // wholly through `histories` — including its clause-less reads
    // elsewhere, which `Literal::pos`/`neg` (as_of: None) already emit
    // correctly, resolving via `naive_eval`'s own default coordinate.
    edb_names.retain(|r| !historical_names.contains(r));

    Ok(Translated {
        program: laws::Program {
            rules,
            fixed: vec![],
            facts: BTreeMap::new(),
            histories: BTreeMap::new(),
        },
        edb_names,
        historical_names,
        entry_rel,
    })
}

/// Scan every EDB relation the translated program reads, at the query's
/// current-state coordinate, through the SAME scan operator the production
/// compiler builds for a stored-relation atom
/// ([`StoredWithValidityRA::iter_batched`]) — not a second, bespoke decode
/// path. `tx` is the one shared snapshot both evaluators read.
fn scan_edb_facts(
    tx: &impl ReadTx,
    edb_names: &BTreeSet<laws::Rel>,
    cur_vld: ValidityTs,
) -> Result<BTreeMap<laws::Rel, BTreeSet<Tuple>>> {
    let as_of = AsOf::current(cur_vld);
    let mut facts = BTreeMap::new();
    for rel in edb_names {
        let handle = get_relation(tx, rel.as_str())?;
        let arity = handle.arity();
        let bindings: Vec<Symbol> = (0..arity)
            .map(|i| Symbol::new(format!("_verify_{i}"), kyzo_model::SourceSpan(0, 0)))
            .collect();
        let scan = StoredWithValidityRA {
            bindings,
            storage: handle,
            filters: vec![],
            as_of,
            span: kyzo_model::SourceSpan(0, 0),
        };
        let mut rows: BTreeSet<Tuple> = BTreeSet::new();
        for batch in scan.iter_batched(tx)? {
            for row in batch?.into_rows() {
                rows.insert(row);
            }
        }
        facts.insert(rel.clone(), rows);
    }
    Ok(facts)
}

/// Scan every HISTORICAL relation's COMPLETE version history — every
/// assert/retract/erase ever written, unresolved against any coordinate —
/// through the exact raw decoder `query/ra/temporal.rs`'s `@spans`/`@delta`
/// operators already use (`decode_raw_version`, story #62 chunk 3): reused,
/// not re-derived, so a bitemporal-tail decoding bug is shared rather than
/// independently risked twice. `tx` is the SAME shared snapshot every other
/// scan in this module reads.
fn scan_full_histories(
    tx: &impl ReadTx,
    historical_names: &BTreeSet<laws::Rel>,
) -> Result<BTreeMap<laws::Rel, Vec<laws::Event>>> {
    use crate::query::ra::temporal::{decode_raw_version, relation_keyspace_bounds};

    let mut histories = BTreeMap::new();
    for rel in historical_names {
        let handle = get_relation(tx, rel.as_str())?;
        let key_len = handle.metadata.keys.len();
        let (lower, upper) = relation_keyspace_bounds(&handle);
        let mut events = Vec::new();
        for kv in tx.range_scan(&lower, &upper) {
            let (key, val) = kv?;
            let (_, key_tuple, raw) = decode_raw_version(&key, &val, key_len)?;
            let event = match raw {
                crate::query::ra::temporal::RawVersion::Assert {
                    valid,
                    sys,
                    payload,
                } => laws::Event::assert(key_tuple, payload, valid.raw(), sys.raw())?,
                crate::query::ra::temporal::RawVersion::Retract { valid, sys } => {
                    laws::Event::retract(key_tuple, valid.raw(), sys.raw())?
                }
                crate::query::ra::temporal::RawVersion::Erase { valid, sys } => {
                    laws::Event::erase(key_tuple, valid.raw(), sys.raw())?
                }
            };
            events.push(event);
        }
        histories.insert(rel.clone(), events);
    }
    Ok(histories)
}

/// The oracle's own [`laws::naive_eval_at_budgeted`] budget, built from the
/// SAME [`ScriptOptions`] the production side already used for
/// [`Db::compile_and_eval`], applying the SAME defaults when the caller
/// leaves a dimension unset — the epoch AND derived-tuple ceilings and the
/// `:timeout`/`timeout_secs` deadline apply to the reference path exactly as
/// they do to production's, so a hostile or merely large query (including a
/// widening recursion under fully default options) cannot OOM or hang
/// `::verify` even though it runs BOTH evaluators. This does not lean on
/// production being evaluated first: the oracle carries its own finite
/// ceiling regardless of evaluation order. No kill flag (the verify path has
/// no live session to cancel it from).
fn oracle_budget(options: &ScriptOptions) -> Result<crate::query::eval::Budget> {
    // The default ceilings are the production ones, imported (not re-declared
    // as a local literal) so the oracle path can never silently drift from
    // build_budget's — the exact divergence that once left this path's
    // derived-tuple ceiling unbounded.
    use crate::session::db::{DEFAULT_DERIVED_TUPLE_CEILING, DEFAULT_EPOCH_CEILING};
    let ceiling = options
        .epoch_ceiling
        .unwrap_or(DEFAULT_EPOCH_CEILING)
        .max(1);
    let ceiling = std::num::NonZeroU32::new(ceiling).expect("max(1) is nonzero");
    let mut budget = crate::query::eval::Budget::new(ceiling).with_derived_tuple_ceiling(
        options
            .derived_tuple_ceiling
            .unwrap_or(DEFAULT_DERIVED_TUPLE_CEILING),
    );
    if let Some(secs) = options.timeout_secs.filter(|s| *s > 0.0) {
        let duration = std::time::Duration::try_from_secs_f64(secs)
            .map_err(|_| UnusableTimeout { secs })?;
        budget = budget.with_timeout(duration);
    }
    Ok(budget)
}

impl<S: Storage> Db<S> {
    /// Run `payload` through both the production evaluator and the sealed
    /// naive oracle against one shared snapshot, reporting agreement,
    /// disagreement, or a named refusal. See the module docs for exactly
    /// what this cut translates.
    pub fn verify_script(
        &self,
        payload: &str,
        params: BTreeMap<String, kyzo_model::value::DataValue>,
        options: ScriptOptions,
    ) -> Result<VerifyOutcome> {
        let cur_vld = current_validity()?;
        let fixed = self.fixed_rules();
        let script = parse_script(payload, &params, &fixed, cur_vld)?;
        let program: InputProgram = match script {
            Script::Single(prog) => *prog,
            Script::Sys(_) | Script::Imperative(_) => {
                return Ok(VerifyUnsupported::NotSingleRead.into());
            }
        };
        self.verify_program(program, cur_vld, &options, &|_| {})
    }

    /// The `::verify { ... }` script directive's entry point (`parse/sys.rs`'s
    /// `SysOp::Verify`, dispatched from `run_sys_op`): the query is already
    /// parsed into an `InputProgram` by the time it reaches here — same
    /// shape as `SysOp::Explain`.
    pub(crate) fn verify_input_program(
        &self,
        program: InputProgram,
        cur_vld: ValidityTs,
        options: &ScriptOptions,
    ) -> Result<VerifyOutcome> {
        self.verify_program(program, cur_vld, options, &|_| {})
    }

    /// The shared core behind [`Self::verify_script`] and
    /// [`Self::verify_input_program`], with a hook applied to the oracle's
    /// scanned EDB facts just before evaluation. Production always sees the
    /// real, unsabotaged snapshot — only the oracle's copy is perturbed. The
    /// no-op hook (`&|_| {}`) is what both public entry points use; a
    /// corrupting hook is this module's sabotage proof (`#[cfg(test)]`
    /// below): it simulates "the oracle's snapshot adapter saw a wrong
    /// fact" and proves the comparison surfaces it as a faithful
    /// [`VerifyOutcome::Mismatch`] rather than silently agreeing.
    fn verify_program(
        &self,
        program: InputProgram,
        cur_vld: ValidityTs,
        options: &ScriptOptions,
        sabotage_oracle_facts: &dyn Fn(&mut BTreeMap<laws::Rel, BTreeSet<Tuple>>),
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

        // Captured before `program` is consumed below (`into_normalized_program`
        // takes it by value): the typed program for a MISMATCH bundle —
        // rendered to KyzoScript only in [`VerifyOutcome::into_named_rows`].
        let mismatch_program = MismatchProgram(program.clone());

        let tx = SessionTx::new_read(self.storage.read_tx()?, options.clone());

        // Production: the real pipeline, on this transaction's snapshot.
        let (result, limited, head, _out_opts) = self.compile_and_eval(
            &tx.store,
            &tx.temp,
            program.clone(),
            cur_vld,
            options,
            crate::engines::segments::Segments(Some(&self.segments)),
        )?;
        let _ = limited;
        let _ = &head;
        let production: BTreeSet<Tuple> = result
            .all_iter()?
            .map(TupleInIter::try_into_tuple)
            .collect::<Result<BTreeSet<_>, _>>()?;

        // Oracle: translate, refuse typed, or evaluate on the SAME snapshot.
        let (normalized, _out_opts2) = {
            let view = SessionView {
                store: &tx.store,
                temp: &tx.temp,
            };
            let cancel = CancelFlag::inert();
            let mut normalizer = SessionNormalizer::new(view, cancel);
            program.into_normalized_program(&mut normalizer)?
        };

        let translated = match translate(&normalized) {
            Ok(t) => t,
            Err(e) => {
                return Ok(VerifyUnsupported::Translate(e).into());
            }
        };
        let mut facts = scan_edb_facts(&tx.store, &translated.edb_names, cur_vld)?;
        sabotage_oracle_facts(&mut facts);
        let histories = scan_full_histories(&tx.store, &translated.historical_names)?;
        let mut oracle_program = translated.program;
        oracle_program.facts = facts;
        oracle_program.histories = histories;

        let budget = oracle_budget(options)?;
        match laws::naive_eval_at_budgeted(&oracle_program, laws::AsOf::current(), &budget) {
            Ok(db) => {
                let oracle = db.get(&translated.entry_rel).cloned().unwrap_or_default();
                if oracle == production {
                    Ok(VerifyOutcome::Match {
                        row_count: production.len(),
                    })
                } else {
                    Ok(VerifyOutcome::Mismatch {
                        program: mismatch_program,
                        production,
                        oracle,
                    })
                }
            }
            Err(rejection) => Ok(VerifyOutcome::OracleRefused {
                reason: OracleRefusal::from(rejection),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kyzo_model::value::DataValue;
    use crate::storage::fjall::new_fjall_storage;

    fn no_params() -> BTreeMap<String, kyzo_model::value::DataValue> {
        BTreeMap::new()
    }

    fn seeded_db() -> Db<crate::storage::fjall::FjallStorage> {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = new_fjall_storage(dir.path()).expect("open fjall storage");
        // Leak the tempdir so the store outlives this function — the test
        // only needs the Db handle, never the path.
        std::mem::forget(dir);
        let db = Db::new(storage).expect("open db");
        db.run_script(
            "?[a, b] <- [[1, 2], [2, 3], [3, 4]] :create edge {a, b}",
            no_params(),
        )
        .expect("seed edge");
        db
    }

    const TRANSITIVE_CLOSURE: &str = "path[x, y] := *edge[x, y]
         path[x, z] := path[x, y], *edge[y, z]
         ?[x, y] := path[x, y]";

    /// Parse `payload` into a single read `InputProgram`, exactly as
    /// [`Db::verify_script`] does, for tests exercising [`Db::verify_program`]
    /// (the private core) directly.
    fn parse_single(payload: &str) -> (InputProgram, ValidityTs) {
        let cur_vld = current_validity().expect("mint a validity stamp");
        let fixed = std::collections::BTreeMap::new();
        match parse_script(payload, &no_params(), &fixed, cur_vld).expect("script parses") {
            Script::Single(prog) => (*prog, cur_vld),
            Script::Imperative(_) | Script::Sys(_) => panic!("expected a single query script"),
        }
    }

    /// The MATCH case: a real recursive query (transitive closure) agrees
    /// between the production evaluator and the oracle.
    #[test]
    fn verify_matches_on_a_real_recursive_query() {
        let db = seeded_db();
        let outcome = db
            .verify_script(TRANSITIVE_CLOSURE, no_params(), ScriptOptions::default())
            .expect("verify_script runs");
        match outcome {
            VerifyOutcome::Match { row_count } => {
                // edge is a 3-hop chain 1->2->3->4: transitive closure has
                // 6 pairs (1,2)(1,3)(1,4)(2,3)(2,4)(3,4).
                assert_eq!(row_count, 6, "unexpected row count for the seeded chain");
            }
            other @ (VerifyOutcome::Mismatch { .. }
            | VerifyOutcome::Unsupported { .. }
            | VerifyOutcome::OracleRefused { .. }) => panic!("expected Match, got {other:?}"),
        }
    }

    /// The sabotage proof: a deliberately corrupted oracle-side fact must
    /// surface as a faithful MISMATCH carrying BOTH answer sets, never a
    /// silent agreement and never a panic.
    #[test]
    fn verify_catches_a_deliberately_sabotaged_oracle_fact() {
        let db = seeded_db();
        let (program, cur_vld) = parse_single(TRANSITIVE_CLOSURE);
        let outcome = db
            .verify_program(program, cur_vld, &ScriptOptions::default(), &|facts| {
                // Drop one real edge from the ORACLE's view only —
                // production still sees the true snapshot. The oracle's
                // transitive closure now lacks every pair that routed
                // through the dropped edge (3,4): (1,4)(2,4)(3,4) go
                // missing from its answer, so it MUST disagree with
                // production's true (unsabotaged) answer.
                facts
                    .get_mut("edge")
                    .expect("edge was scanned")
                    .remove(&Tuple::from_vec(vec![
                        DataValue::from(3),
                        DataValue::from(4),
                    ]));
            })
            .expect("verify_program runs");
        match outcome {
            VerifyOutcome::Mismatch {
                production, oracle, ..
            } => {
                assert_eq!(production.len(), 6, "production must be the true answer");
                assert!(
                    oracle.len() < production.len(),
                    "the sabotaged oracle must be missing rows: {oracle:?}"
                );
                let dropped: Tuple = Tuple::from_vec(vec![DataValue::from(3), DataValue::from(4)]);
                assert!(
                    !oracle.contains(&dropped) || oracle.len() != production.len(),
                    "sabotage must be visible in the oracle's answer"
                );
            }
            other @ (VerifyOutcome::Match { .. }
            | VerifyOutcome::Unsupported { .. }
            | VerifyOutcome::OracleRefused { .. }) => panic!("expected Mismatch, got {other:?}"),
        }
    }

    /// A construct outside this cut's scope (a predicate/filter atom) is a
    /// named, typed refusal — never a silent pass, never a crash.
    #[test]
    fn verify_refuses_a_predicate_atom_by_name() {
        let db = seeded_db();
        let outcome = db
            .verify_script(
                "?[x, y] := *edge[x, y], y > 2",
                no_params(),
                ScriptOptions::default(),
            )
            .expect("verify_script runs");
        match outcome {
            VerifyOutcome::Unsupported {
                reason: VerifyUnsupported::Translate(TranslateUnsupported::Predicate),
            } => {}
            other @ (VerifyOutcome::Match { .. }
            | VerifyOutcome::Mismatch { .. }
            | VerifyOutcome::Unsupported { .. }
            | VerifyOutcome::OracleRefused { .. }) => panic!("expected Unsupported(Predicate), got {other:?}"),
        }
    }

    /// The product surface itself: `::verify { ... }` invoked as an ordinary
    /// KyzoScript directive through `Db::run_script` — not the Rust API —
    /// proving `SysOp::Verify`'s grammar and dispatch end to end.
    #[test]
    fn verify_directive_runs_through_run_script() {
        let db = seeded_db();
        let rows = db
            .run_script(
                "::verify { path[x, y] := *edge[x, y]
                 path[x, z] := path[x, y], *edge[y, z]
                 ?[x, y] := path[x, y] }",
                no_params(),
            )
            .expect("::verify runs as a script directive");
        assert_eq!(
            rows.headers(),
            &["status".to_string(), "summary".to_string(), "detail".to_string()]
        );
        assert_eq!(rows.rows().len(), 1);
        assert_eq!(rows.rows()[0][0], DataValue::from("match"));
    }

    /// The directive surfaces an unsupported construct the same way the
    /// Rust API does, reached through `::verify`'s own dispatch path
    /// (`SysOp::Verify` -> `into_named_rows`), not `verify_program` directly.
    #[test]
    fn verify_directive_names_unsupported_constructs() {
        let db = seeded_db();
        let rows = db
            .run_script("::verify { ?[x, y] := *edge[x, y], y > 2 }", no_params())
            .expect("::verify runs as a script directive");
        assert_eq!(rows.rows()[0][0], DataValue::from("unsupported"));
    }

    // ════════════════════════════════════════════════════════════════════
    // The whole-corpus proof (story #80 DoD): breadth, not three hand-picked
    // cases. Reuses `query/gauntlet.rs`'s (issue #29) `laws::Program` ->
    // KyzoScript-text generator and renderer directly — "reused, not
    // re-derived," the same principle that module's own refusal-fence test
    // states for itself — rather than hand-rolling a second corpus.
    // ════════════════════════════════════════════════════════════════════

    /// Every accepted query in a wide, seeded, randomly generated corpus
    /// (linear/self-join recursion, optional negation-over-recursion, swept
    /// over many graph shapes) returns `Match` through `::verify`. Aggregation
    /// is NOT exercised by this generator (`gauntlet::gen_program`'s own
    /// documented scope) — covered separately by
    /// `verify_matches_a_hand_written_aggregation_query` below.
    #[test]
    fn verify_matches_across_a_generated_corpus() {
        use crate::query::gauntlet::{Rng, entry_line, facts_script, gen_program, rules_script};

        const SEEDS: u64 = 40;
        let mut failures = Vec::new();
        for seed in 0..SEEDS {
            let mut rng = Rng::new(seed);
            let (program, entries) = gen_program(&mut rng);
            let db =
                Db::new(crate::storage::sim::SimStorage::new(seed)).expect("open sim-backed db");
            for (rel, rows) in &program.facts {
                let arity = rows.iter().next().map(|t| t.len()).unwrap_or(0);
                db.run_script(&facts_script(rel, arity, rows), no_params())
                    .unwrap_or_else(|e| panic!("seed {seed}: fact load for {rel}: {e}"));
            }
            let rules_text = rules_script(&program);
            for (entry_rel, arity) in entries {
                let line = entry_line(entry_rel, &vec![None; arity]);
                let script = format!("::verify {{ {rules_text}\n{line} }}");
                match db.run_script(&script, no_params()) {
                    Ok(rows) if rows.rows()[0][0] == DataValue::from("match") => {}
                    Ok(rows) => failures.push(format!(
                        "seed {seed} entry {entry_rel}: expected match, got {:?}",
                        rows.rows()[0]
                    )),
                    Err(e) => failures.push(format!("seed {seed} entry {entry_rel}: {e}")),
                }
            }
        }
        assert!(
            failures.is_empty(),
            "generated-corpus verify FINDINGS ({} of {SEEDS} seeds):\n{}",
            failures.len(),
            failures.join("\n")
        );
    }

    /// Aggregation (normal + meet), the one construct `gen_program` never
    /// emits, still `Match`es — closing the gap the generated corpus above
    /// leaves named.
    #[test]
    fn verify_matches_a_hand_written_aggregation_query() {
        let db = seeded_db();
        let outcome = db
            .verify_script(
                "?[y, count(x)] := *edge[x, y]",
                no_params(),
                ScriptOptions::default(),
            )
            .expect("verify_script runs");
        match outcome {
            VerifyOutcome::Match { .. } => {}
            other @ (VerifyOutcome::Mismatch { .. }
            | VerifyOutcome::Unsupported { .. }
            | VerifyOutcome::OracleRefused { .. }) => panic!("expected Match, got {other:?}"),
        }
    }

    /// The refusal-corpus proof (story #80 DoD): `laws::unstratifiable_corpus()`
    /// — the SAME hand-built corpus `query/gauntlet.rs`'s refusal fence
    /// already proves the real engine rejects — must never `Match` through
    /// `::verify` either. Every entry ends in one of two honest outcomes: the
    /// production compiler itself refuses first (a hard `Err`, propagated
    /// before `::verify`'s own comparison ever runs), or it reaches the
    /// comparison and is named `Unsupported`/`OracleRefused` — never silent
    /// agreement.
    #[test]
    fn verify_never_matches_the_unstratifiable_corpus() {
        use crate::query::gauntlet::{edb_relations, entry_line, facts_script, rules_script};
        use crate::query::laws::unstratifiable_corpus;

        let mut failures = Vec::new();
        for (name, program) in unstratifiable_corpus() {
            if !program.fixed.is_empty() {
                // Same documented skip as gauntlet.rs's refusal fence: a
                // fixed rule here models an opaque Rust closure with no
                // KyzoScript syntax to invoke an unregistered algorithm.
                continue;
            }
            let db = Db::new(crate::storage::sim::SimStorage::new(0xC09A))
                .unwrap_or_else(|e| panic!("{name}: db open: {e}"));
            for (rel, arity) in edb_relations(&program) {
                db.run_script(&facts_script(rel, arity, &BTreeSet::new()), no_params())
                    .unwrap_or_else(|e| panic!("{name}: create EDB {rel}: {e}"));
            }
            let rules_text = rules_script(&program);
            let heads: BTreeSet<&str> = program.rules.iter().map(|r| r.head_rel).collect();
            for rel in heads {
                let arity = program
                    .rules
                    .iter()
                    .find(|r| r.head_rel == rel)
                    .expect("rel came from this program's own heads")
                    .head_args
                    .len();
                let line = entry_line(rel, &vec![None; arity]);
                let script = format!("::verify {{ {rules_text}\n{line} }}");
                match db.run_script(&script, no_params()) {
                    // The production compiler refused before ::verify's own
                    // comparison ran — a legitimate "stays refused" outcome.
                    Err(_) => {}
                    Ok(rows) if rows.rows()[0][0] != DataValue::from("match") => {}
                    Ok(rows) => failures.push(format!(
                        "{name}/{rel}: ::verify silently matched an unstratifiable \
                         program: {:?}",
                        rows.rows()[0]
                    )),
                }
            }
        }
        assert!(
            failures.is_empty(),
            "refusal-corpus verify FINDINGS:\n{}",
            failures.join("\n")
        );
    }

    // ════════════════════════════════════════════════════════════════════
    // Time travel (story #80 DoD item 3, ruled required, not follow-on):
    // point-in-time (`@ <coordinate>`) historical reads, translated through
    // `laws::Program::histories` and the shared full-history decoder
    // (`query/ra/temporal.rs`'s `decode_raw_version`).
    // ════════════════════════════════════════════════════════════════════

    /// Two versions of the same fact, written at two different valid
    /// instants: `::verify` must agree with production at EACH instant
    /// independently — the oracle resolving from the full history, not a
    /// single precomputed snapshot.
    #[test]
    fn verify_matches_a_point_in_time_historical_read() {
        let dir = tempfile::tempdir().unwrap();
        let storage = new_fjall_storage(dir.path()).unwrap();
        std::mem::forget(dir);
        let db = Db::new(storage).unwrap();
        db.run_script(
            "?[k, v] <- [[1, 'a']] :create hist {k => v} @ 100",
            no_params(),
        )
        .expect("create at valid=100");
        db.run_script(
            "?[k, v] <- [[1, 'b']] :put hist {k => v} @ 200",
            no_params(),
        )
        .expect("put at valid=200");

        let at_100 = db
            .verify_script(
                "?[k, v] := *hist[k, v @ 100]",
                no_params(),
                ScriptOptions::default(),
            )
            .expect("verify_script runs");
        match at_100 {
            VerifyOutcome::Match { row_count } => assert_eq!(row_count, 1),
            other @ (VerifyOutcome::Mismatch { .. }
            | VerifyOutcome::Unsupported { .. }
            | VerifyOutcome::OracleRefused { .. }) => panic!("expected Match at valid=100, got {other:?}"),
        }

        let at_200 = db
            .verify_script(
                "?[k, v] := *hist[k, v @ 200]",
                no_params(),
                ScriptOptions::default(),
            )
            .expect("verify_script runs");
        match at_200 {
            VerifyOutcome::Match { row_count } => assert_eq!(row_count, 1),
            other @ (VerifyOutcome::Mismatch { .. }
            | VerifyOutcome::Unsupported { .. }
            | VerifyOutcome::OracleRefused { .. }) => panic!("expected Match at valid=200, got {other:?}"),
        }

        // Before the fact existed at all: empty, not a refusal.
        let at_50 = db
            .verify_script(
                "?[k, v] := *hist[k, v @ 50]",
                no_params(),
                ScriptOptions::default(),
            )
            .expect("verify_script runs");
        match at_50 {
            VerifyOutcome::Match { row_count } => assert_eq!(row_count, 0),
            other @ (VerifyOutcome::Mismatch { .. }
            | VerifyOutcome::Unsupported { .. }
            | VerifyOutcome::OracleRefused { .. }) => panic!("expected an empty Match at valid=50, got {other:?}"),
        }
    }

    /// A negated historical read (`not *hist[... @ <coordinate>]`) — the
    /// same `Literal::neg_at` path — still matches. Both of `hist`'s
    /// columns are bound by a POSITIVE atom first (`*probe[k, v]`) so the
    /// negation is unambiguously safe under any definition — a
    /// wildcard-only-in-negation column is a distinct, separately named
    /// boundary (see the module docs), not what this test is proving.
    #[test]
    fn verify_matches_a_negated_historical_read() {
        let dir = tempfile::tempdir().unwrap();
        let storage = new_fjall_storage(dir.path()).unwrap();
        std::mem::forget(dir);
        let db = Db::new(storage).unwrap();
        db.run_script(
            "?[k, v] <- [[1, 'a']] :create hist {k => v} @ 100",
            no_params(),
        )
        .expect("create at valid=100");
        db.run_script(
            "?[k, v] <- [[1, 'a'], [2, 'z']] :create probe {k => v}",
            no_params(),
        )
        .expect("create probe");

        let outcome = db
            .verify_script(
                "?[k, v] := *probe[k, v], not *hist[k, v @ 50]",
                no_params(),
                ScriptOptions::default(),
            )
            .expect("verify_script runs");
        match outcome {
            // At valid=50 nothing existed in hist yet, so both probe pairs
            // pass the negation.
            VerifyOutcome::Match { row_count } => assert_eq!(row_count, 2),
            other @ (VerifyOutcome::Mismatch { .. }
            | VerifyOutcome::Unsupported { .. }
            | VerifyOutcome::OracleRefused { .. }) => panic!("expected Match, got {other:?}"),
        }
    }

    /// The interval-derivation/diff boundary, named specifically (not a
    /// generic "time travel unsupported"): `@spans` binds an extra column
    /// beyond the relation's own arity, a distinct shape from the
    /// point-in-time `@` case above.
    #[test]
    fn verify_refuses_a_spans_read_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let storage = new_fjall_storage(dir.path()).unwrap();
        std::mem::forget(dir);
        let db = Db::new(storage).unwrap();
        db.run_script(
            "?[k, v] <- [[1, 'a']] :create hist {k => v} @ 100",
            no_params(),
        )
        .expect("create at valid=100");

        let outcome = db
            .verify_script(
                "?[k, v, iv] := *hist{k, v @spans iv}",
                no_params(),
                ScriptOptions::default(),
            )
            .expect("verify_script runs");
        match outcome {
            VerifyOutcome::Unsupported {
                reason:
                    VerifyUnsupported::Translate(TranslateUnsupported::IntervalDerivation { name }),
            } => {
                assert!(
                    name.contains("hist"),
                    "expected the hist relation in the @spans refusal, got: {name}"
                );
            }
            other @ (VerifyOutcome::Match { .. }
            | VerifyOutcome::Mismatch { .. }
            | VerifyOutcome::Unsupported { .. }
            | VerifyOutcome::OracleRefused { .. }) => {
                panic!("expected Unsupported(IntervalDerivation), got {other:?}")
            }
        }
    }

    // ════════════════════════════════════════════════════════════════════
    // Budgeted oracle execution (story #80 DoD item 2, additive): the SAME
    // ScriptOptions ceiling production already respects now bounds the
    // oracle's naive fixpoint too, so ::verify cannot hang or OOM on a
    // hostile/large query.
    // ════════════════════════════════════════════════════════════════════

    /// A real recursive program under a deliberately starved epoch ceiling:
    /// `::verify` applies ONE caller-given `ScriptOptions` ceiling to BOTH
    /// evaluators (by design — a caller who asked for a tight budget wants
    /// it enforced everywhere, not just on the side that happens to hit it
    /// first), so here production's own budget (`Db::compile_and_eval`,
    /// unchanged, already ceiling-checked) refuses before the oracle
    /// comparison ever runs — an ordinary, correctly propagated `Err`, not
    /// a silent hang or a wrong answer. The oracle's OWN budget mechanism
    /// in isolation (a case where the oracle alone starves) is proven
    /// directly against `naive_eval_at_budgeted` in `query/laws.rs`'s test
    /// module, where production's independent budget cannot confound it.
    #[test]
    fn verify_propagates_a_starved_epoch_ceiling_as_an_ordinary_refusal() {
        let db = seeded_db();
        // A ceiling of 1 refuses every non-empty program deterministically
        // (Budget::new's own doc: one round derives, a second observes the
        // empty delta) — the recursive TRANSITIVE_CLOSURE program needs
        // several, on EITHER evaluator.
        let options = ScriptOptions {
            epoch_ceiling: Some(1),
            ..ScriptOptions::default()
        };
        let err = db
            .verify_script(TRANSITIVE_CLOSURE, no_params(), options)
            .expect_err("a starved ceiling must refuse, not hang or silently truncate");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("epoch") || msg.contains("Epochs"),
            "expected an epoch-ceiling refusal, got: {msg}"
        );
    }

    /// A generous ceiling on the SAME program still matches — the budget
    /// changes nothing about the answer when it is never crossed.
    #[test]
    fn verify_still_matches_under_a_generous_budget() {
        let db = seeded_db();
        let options = ScriptOptions {
            epoch_ceiling: Some(1_000),
            derived_tuple_ceiling: Some(10_000),
            ..ScriptOptions::default()
        };
        let outcome = db
            .verify_script(TRANSITIVE_CLOSURE, no_params(), options)
            .expect("verify_script runs");
        match outcome {
            VerifyOutcome::Match { row_count } => assert_eq!(row_count, 6),
            other @ (VerifyOutcome::Mismatch { .. }
            | VerifyOutcome::Unsupported { .. }
            | VerifyOutcome::OracleRefused { .. }) => panic!("expected Match, got {other:?}"),
        }
    }

    /// Regression: the oracle's budget must carry the SAME finite
    /// derived-tuple default the production path does when the caller leaves
    /// it unset. This path was once unbounded — a widening recursion run
    /// through `::verify` under default options had no derived-tuple ceiling
    /// on the naive oracle, and did not crash only because production is
    /// evaluated first and trips its own ceiling. The default now comes from
    /// the one shared `db::DEFAULT_DERIVED_TUPLE_CEILING` constant, so the two
    /// paths cannot drift apart again. Asserted at budget construction rather
    /// than by tripping the 50M ceiling end to end (which would cost seconds
    /// and gigabytes for no extra coverage of THIS guarantee).
    #[test]
    fn oracle_budget_defaults_derived_tuple_ceiling_like_production() {
        use crate::session::db::DEFAULT_DERIVED_TUPLE_CEILING;

        // Default options (derived_tuple_ceiling: None) → the production default.
        let defaulted = oracle_budget(&ScriptOptions::default()).expect("budget builds");
        assert_eq!(
            defaulted.derived_tuple_ceiling(),
            Some(DEFAULT_DERIVED_TUPLE_CEILING),
            "the oracle path must default the derived-tuple ceiling, never run unbounded"
        );

        // An explicit override still wins on the oracle path.
        let overridden = oracle_budget(&ScriptOptions {
            derived_tuple_ceiling: Some(4_242),
            ..ScriptOptions::default()
        })
        .expect("budget builds");
        assert_eq!(overridden.derived_tuple_ceiling(), Some(4_242));
    }
}
