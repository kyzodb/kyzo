/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Oracle-differential verify corpus — re-homed from condemned
//! `kyzo-core::session::verify` (pre-`3f8749b` query-answer half).
//!
//! Production `::verify` stays [`kyzo::EngineRefuse`]-class
//! `IndexOpNotLanded` (disclosed [OPEN] to #257/#258 provenance). This
//! module is the zone-trials home for the oracle-vs-engine differential:
//! [`Engine::run_script_with`] on one side, [`kyzo_oracle::naive_eval_at_budgeted`]
//! on the other, against EDB facts loaded through public script doors.
//!
//! Root tamper-evidence verify (story #289) stays in `kyzo-core` — not here.
//!
//! ## Public-door boundary
//!
//! Translation walks [`kyzo_model::program::InputProgram`] (language door),
//! not `NormalFormProgram` (exec-internal). EDB current-state facts are
//! loaded via `::columns` + `?[…] := *rel[…]`. Full raw-version history
//! scan (`decode_raw_version`) is still kyzo-core-internal — historical
//! differentials use [`verify_script_with_histories`] with caller-supplied
//! [`Event`]s until a public history door lands.

#![cfg(test)]

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;

use miette::{Diagnostic, Result};
use thiserror::Error;

use kyzo::{Catalog, Engine, NamedRows, ScriptOptions, Storage, new_fjall_storage};
use kyzo_model::parse::{Script, parse_script};
use kyzo_model::program::expr::Expr;
use kyzo_model::program::rule::{
    HeadAggrSlot, InputAtom, InputInlineRulesOrFixed, InputProgram, ValidityClause,
};
use kyzo_model::program::symbol::Symbol;
use kyzo_model::value::{DataValue, Tuple};
use kyzo_oracle::eval::{
    self as laws, HeadAggr, Literal, Name, OracleBudget, Program, Rejection, Rel, Rule, Term,
};
use kyzo_oracle::{AsOf as OracleAsOf, Event, builtin_fold, naive_eval_at_budgeted};

/// Must match `kyzo::session::db::DEFAULT_EPOCH_CEILING` (pub(crate) there).
const DEFAULT_EPOCH_CEILING: u32 = 1_000_000;
/// Must match `kyzo::session::db::DEFAULT_DERIVED_TUPLE_CEILING` (pub(crate) there).
const DEFAULT_DERIVED_TUPLE_CEILING: u64 = 50_000_000;

/// Outcome of one differential verify run.
#[derive(Debug, Clone)]
pub enum VerifyOutcome {
    Match { row_count: usize },
    Mismatch {
        program: MismatchProgram,
        production: BTreeSet<Tuple>,
        oracle: BTreeSet<Tuple>,
    },
    Unsupported { reason: VerifyUnsupported },
    OracleRefused { reason: OracleRefusal },
}

#[derive(Debug, Clone)]
pub struct MismatchProgram(pub InputProgram);

impl std::fmt::Display for MismatchProgram {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OracleRefusal(pub Rejection);

impl From<Rejection> for OracleRefusal {
    fn from(rejection: Rejection) -> Self {
        OracleRefusal(rejection)
    }
}

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

#[derive(Debug, Clone, PartialEq, Eq, Error, Diagnostic)]
pub enum TranslateUnsupported {
    #[error(
        "relation atom '{name}' is an interval-derivation (@spans) or diff \
         (@delta/@delta_sys) read: these bind an extra column beyond the \
         relation's own arity, a distinct translator shape from the \
         point-in-time @ read — not yet translated"
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
    #[error(
        "named-field relation atoms need schema column order to positionalize; \
         trials translate positional `*rel[…]` only until a public catalog-order \
         door feeds the translator"
    )]
    #[diagnostic(code(verify::translate::named_field))]
    NamedField { name: Symbol },
    #[error("disjunction body atoms are not flattened here — normalize first")]
    #[diagnostic(code(verify::translate::disjunction))]
    Disjunction,
    #[error(
        "historical @ reads need a full-history EDB feed; Engine exposes no public \
         raw-version scan yet (decode_raw_version remains kyzo-core-internal) — \
         use verify_script_with_histories"
    )]
    #[diagnostic(code(verify::translate::full_history_scan))]
    FullHistoryScanNotPublic,
}

impl From<VerifyUnsupported> for VerifyOutcome {
    fn from(reason: VerifyUnsupported) -> Self {
        VerifyOutcome::Unsupported { reason }
    }
}

#[derive(Debug, Clone, PartialEq, Error, Diagnostic)]
#[error("timeout {secs} is not a usable duration")]
#[diagnostic(code(verify::unusable_timeout))]
struct UnusableTimeout {
    secs: f64,
}

fn oracle_name(sym: &Symbol) -> Name {
    Name::owned(sym.name.as_str())
}

struct Translated {
    program: Program,
    edb_names: BTreeSet<Rel>,
    historical_names: BTreeSet<Rel>,
    entry_rel: Rel,
}

fn translate_term_sym(sym: &Symbol) -> Term {
    Term::var(oracle_name(sym))
}

fn translate_expr_term(expr: &Expr) -> std::result::Result<Term, TranslateUnsupported> {
    match expr {
        Expr::Binding { var, .. } => Ok(translate_term_sym(var)),
        Expr::Const { val, .. } => Ok(Term::Const(val.clone())),
        _ => Err(TranslateUnsupported::Predicate),
    }
}

fn to_oracle_asof(real: kyzo_model::value::AsOf) -> OracleAsOf {
    OracleAsOf {
        valid: real.valid().raw(),
        sys: real.sys().raw(),
    }
}

fn translate_aggr(slot: &HeadAggrSlot) -> std::result::Result<HeadAggr, TranslateUnsupported> {
    match slot {
        HeadAggrSlot::Plain => Ok(HeadAggr::Plain),
        HeadAggrSlot::Aggregated { aggr, args } => {
            let fold = builtin_fold(aggr.name).ok_or(TranslateUnsupported::Predicate)?;
            Ok(HeadAggr::Aggregated {
                fold,
                args: args.clone(),
            })
        }
    }
}

fn negate_literal(lit: Literal) -> Literal {
    use laws::Polarity;
    Literal {
        polarity: match lit.polarity {
            Polarity::Positive => Polarity::Negative,
            Polarity::Negative => Polarity::Positive,
        },
        ..lit
    }
}

fn translate_body_atom(
    atom: &InputAtom,
    edb_names: &mut BTreeSet<Rel>,
    historical_names: &mut BTreeSet<Rel>,
) -> std::result::Result<Vec<Literal>, TranslateUnsupported> {
    match atom {
        InputAtom::Rule { inner } => {
            let args = inner
                .args
                .iter()
                .map(translate_expr_term)
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(vec![Literal::pos(oracle_name(&inner.name), args)])
        }
        InputAtom::Relation { inner } => {
            let rel = oracle_name(&inner.name);
            let args = inner
                .args
                .iter()
                .map(translate_expr_term)
                .collect::<std::result::Result<Vec<_>, _>>()?;
            match &inner.validity {
                None => {
                    edb_names.insert(rel.clone());
                    Ok(vec![Literal::pos(rel, args)])
                }
                Some(ValidityClause::At(as_of)) => {
                    historical_names.insert(rel.clone());
                    Ok(vec![Literal::pos_at(rel, args, to_oracle_asof(*as_of))])
                }
                Some(ValidityClause::Spans { .. } | ValidityClause::Delta { .. }) => {
                    Err(TranslateUnsupported::IntervalDerivation {
                        name: inner.name.clone(),
                    })
                }
            }
        }
        InputAtom::NamedFieldRelation { inner } => match &inner.validity {
            Some(ValidityClause::Spans { .. } | ValidityClause::Delta { .. }) => {
                Err(TranslateUnsupported::IntervalDerivation {
                    name: inner.name.clone(),
                })
            }
            _ => Err(TranslateUnsupported::NamedField {
                name: inner.name.clone(),
            }),
        },
        InputAtom::Negation { inner, .. } => {
            let mut lits = translate_body_atom(inner, edb_names, historical_names)?;
            if lits.len() != 1 {
                return Err(TranslateUnsupported::Disjunction);
            }
            Ok(vec![negate_literal(lits.pop().expect("len checked"))])
        }
        InputAtom::Conjunction { inner, .. } => {
            let mut out = Vec::new();
            for a in inner {
                out.extend(translate_body_atom(a, edb_names, historical_names)?);
            }
            Ok(out)
        }
        InputAtom::Disjunction { .. } => Err(TranslateUnsupported::Disjunction),
        InputAtom::Predicate { .. } => Err(TranslateUnsupported::Predicate),
        InputAtom::Unification { .. } => Err(TranslateUnsupported::Unification),
        InputAtom::Search { .. } => Err(TranslateUnsupported::Search),
    }
}

fn translate_inline_rule(
    head_rel: Rel,
    rule: &kyzo_model::program::rule::InputInlineRule,
    edb_names: &mut BTreeSet<Rel>,
    historical_names: &mut BTreeSet<Rel>,
) -> std::result::Result<Rule, TranslateUnsupported> {
    let head_args: Vec<Term> = rule.head.iter().map(translate_term_sym).collect();
    let aggr = rule
        .aggr
        .iter()
        .map(translate_aggr)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut body = Vec::new();
    for a in &rule.body {
        body.extend(translate_body_atom(a, edb_names, historical_names)?);
    }
    Ok(Rule::aggregated(head_rel, head_args, aggr, body))
}

fn translate_def(
    name: &Symbol,
    def: &InputInlineRulesOrFixed,
    rules: &mut Vec<Rule>,
    edb_names: &mut BTreeSet<Rel>,
    historical_names: &mut BTreeSet<Rel>,
) -> std::result::Result<(), TranslateUnsupported> {
    match def {
        InputInlineRulesOrFixed::Fixed { fixed } => Err(TranslateUnsupported::FixedRule {
            rel: name.clone(),
            fixed: fixed.fixed_handle.name.clone(),
        }),
        InputInlineRulesOrFixed::Rules { rules: inline } => {
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

fn translate(program: &InputProgram) -> std::result::Result<Translated, TranslateUnsupported> {
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
    edb_names.retain(|r| !historical_names.contains(r));

    Ok(Translated {
        program: Program {
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

fn oracle_budget(options: &ScriptOptions) -> Result<OracleBudget> {
    let ceiling = options
        .epoch_ceiling
        .unwrap_or(DEFAULT_EPOCH_CEILING)
        .max(1);
    let ceiling = NonZeroU32::new(ceiling).expect("max(1) is nonzero");
    let mut budget = OracleBudget::new(ceiling).with_derived_tuple_ceiling(
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

fn no_params() -> BTreeMap<String, DataValue> {
    BTreeMap::new()
}

fn relation_arity<S: Storage>(db: &Engine<S>, rel: &str) -> Result<usize> {
    let rows = db.run_script(&format!("::columns {rel}"), no_params())?;
    Ok(rows.rows().len())
}

fn scan_edb_facts<S: Storage>(
    db: &Engine<S>,
    edb_names: &BTreeSet<Rel>,
) -> Result<BTreeMap<Rel, BTreeSet<Tuple>>> {
    let mut facts = BTreeMap::new();
    for rel in edb_names {
        let arity = relation_arity(db, rel.as_str())?;
        let vars: Vec<String> = (0..arity).map(|i| format!("c{i}")).collect();
        let cols = vars.join(", ");
        let q = format!("?[{cols}] := *{rel}[{cols}]");
        let rows = db.run_script(&q, no_params())?;
        let set: BTreeSet<Tuple> = rows.rows().iter().cloned().collect();
        facts.insert(rel.clone(), set);
    }
    Ok(facts)
}

fn named_rows_set(rows: &NamedRows) -> BTreeSet<Tuple> {
    rows.rows().iter().cloned().collect()
}

fn open_engine<S: Storage>(store: S) -> Engine<S> {
    Engine::compose(store, Catalog::new()).expect("compose engine")
}

fn seeded_db() -> Engine<kyzo::FjallStorage> {
    let dir = tempfile::tempdir().expect("tempdir");
    let storage = new_fjall_storage(dir.path()).expect("open fjall storage");
    std::mem::forget(dir);
    let db = open_engine(storage);
    db.run_script(":create edge {a: Int, b: Int}", no_params())
        .expect("create edge schema");
    let rows = DataValue::List(vec![
        DataValue::List(vec![DataValue::from(1i64), DataValue::from(2i64)]),
        DataValue::List(vec![DataValue::from(2i64), DataValue::from(3i64)]),
        DataValue::List(vec![DataValue::from(3i64), DataValue::from(4i64)]),
    ]);
    db.run_script(
        "?[a, b] <- $rows :put edge {a, b}",
        BTreeMap::from([("rows".into(), rows)]),
    )
    .expect("seed edge");
    db
}

/// Differential verify: production [`Engine::run_script_with`] vs oracle
/// naive eval. Historical `@` programs must use
/// [`verify_script_with_histories`] until a public history-scan door exists.
pub fn verify_script<S: Storage>(
    db: &Engine<S>,
    payload: &str,
    params: BTreeMap<String, DataValue>,
    options: ScriptOptions,
) -> Result<VerifyOutcome> {
    verify_script_inner(db, payload, params, options, None, &|_| {})
}

/// Like [`verify_script`], but supplies oracle [`Event`] histories for
/// relations read under `@` (point-in-time) clauses.
pub fn verify_script_with_histories<S: Storage>(
    db: &Engine<S>,
    payload: &str,
    params: BTreeMap<String, DataValue>,
    options: ScriptOptions,
    histories: BTreeMap<Rel, Vec<Event>>,
) -> Result<VerifyOutcome> {
    verify_script_inner(db, payload, params, options, Some(histories), &|_| {})
}

/// Sabotage door: same as [`verify_script`], with a hook that may perturb
/// the oracle's EDB facts after the public scan and before naive eval.
pub fn verify_program_sabotaged<S: Storage>(
    db: &Engine<S>,
    payload: &str,
    params: BTreeMap<String, DataValue>,
    options: ScriptOptions,
    sabotage_oracle_facts: &dyn Fn(&mut BTreeMap<Rel, BTreeSet<Tuple>>),
) -> Result<VerifyOutcome> {
    verify_script_inner(db, payload, params, options, None, sabotage_oracle_facts)
}

fn verify_script_inner<S: Storage>(
    db: &Engine<S>,
    payload: &str,
    params: BTreeMap<String, DataValue>,
    options: ScriptOptions,
    histories_override: Option<BTreeMap<Rel, Vec<Event>>>,
    sabotage_oracle_facts: &dyn Fn(&mut BTreeMap<Rel, BTreeSet<Tuple>>),
) -> Result<VerifyOutcome> {
    let script = parse_script(payload, &params)?;
    let program: InputProgram = match script {
        Script::Query(prog) => prog,
        Script::Sys { .. } | Script::Imperative { .. } => {
            return Ok(VerifyUnsupported::NotSingleRead.into());
        }
    };

    if program.out_opts().store_relation.is_some() {
        return Ok(VerifyUnsupported::Mutation.into());
    }
    if !program.out_opts().sorters.is_empty()
        || program.out_opts().limit.is_some()
        || program.out_opts().offset.is_some()
    {
        return Ok(VerifyUnsupported::OrderLimitOffset.into());
    }

    let mismatch_program = MismatchProgram(program.clone());

    // Production first — budget refusals propagate as ordinary Err.
    let production_rows = db.run_script_with(payload, params, options.clone())?;
    let production = named_rows_set(&production_rows);

    let translated = match translate(&program) {
        Ok(t) => t,
        Err(e) => return Ok(VerifyUnsupported::Translate(e).into()),
    };

    if !translated.historical_names.is_empty() && histories_override.is_none() {
        return Ok(VerifyUnsupported::Translate(
            TranslateUnsupported::FullHistoryScanNotPublic,
        )
        .into());
    }

    let mut facts = scan_edb_facts(db, &translated.edb_names)?;
    sabotage_oracle_facts(&mut facts);
    let histories = histories_override.unwrap_or_default();

    let mut oracle_program = translated.program;
    oracle_program.facts = facts;
    oracle_program.histories = histories;

    let budget = oracle_budget(&options)?;
    match naive_eval_at_budgeted(&oracle_program, OracleAsOf::current(), &budget) {
        Ok(db_out) => {
            let oracle = db_out
                .get(&translated.entry_rel)
                .cloned()
                .unwrap_or_default();
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

// ════════════════════════════════════════════════════════════════════════
// Corpus render helpers (from pre-cut gauntlet) — laws::Program → KyzoScript
// ════════════════════════════════════════════════════════════════════════

struct Rng {
    state: u64,
}

impl Rng {
    fn new(seed: u64) -> Self {
        Rng { state: seed }
    }
    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        debug_assert!(n > 0);
        self.next_u64() % n
    }
    fn range(&mut self, lo: i64, hi: i64) -> i64 {
        debug_assert!(hi > lo);
        lo + self.below((hi - lo) as u64) as i64
    }
    fn chance(&mut self, num: u64, den: u64) -> bool {
        self.below(den) < num
    }
}

fn var(i: usize) -> &'static str {
    const NAMES: [&str; 6] = ["a", "b", "c", "d", "e", "f"];
    NAMES[i]
}

fn term_text(t: &Term) -> String {
    match t {
        Term::Var(v) => v.as_str().to_string(),
        Term::Const(dv) => dv
            .get_int()
            .expect("corpus only mints int constants")
            .to_string(),
    }
}

fn is_idb(program: &Program, rel: &Rel) -> bool {
    program.rules.iter().any(|r| &r.head_rel == rel)
}

fn literal_text(program: &Program, lit: &Literal) -> String {
    let sigil = if is_idb(program, &lit.rel) { "" } else { "*" };
    let args: Vec<String> = lit.args.iter().map(term_text).collect();
    format!(
        "{}{sigil}{}[{}]",
        if lit.is_negated() { "not " } else { "" },
        lit.rel,
        args.join(", ")
    )
}

fn rule_text(program: &Program, rule: &Rule) -> String {
    let head_args: Vec<String> = rule
        .head_args
        .iter()
        .zip(rule.aggr.iter())
        .map(|(t, a)| {
            let base = term_text(t);
            match a {
                HeadAggr::Aggregated { fold, .. } => format!("{}({base})", fold.name()),
                HeadAggr::Plain => base,
            }
        })
        .collect();
    let body: Vec<String> = rule
        .body
        .iter()
        .map(|l| literal_text(program, l))
        .collect();
    format!(
        "{}[{}] := {}",
        rule.head_rel,
        head_args.join(", "),
        body.join(", ")
    )
}

fn rules_script(program: &Program) -> String {
    program
        .rules
        .iter()
        .map(|r| rule_text(program, r))
        .collect::<Vec<_>>()
        .join("\n")
}

fn facts_create_schema(rel: &Rel, arity: usize) -> String {
    let names: Vec<&str> = (0..arity).map(var).collect();
    let cols = names
        .iter()
        .map(|n| format!("{n}: Int"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(":create {rel} {{{cols}}}")
}

fn facts_put_script(rel: &Rel, arity: usize) -> String {
    let names: Vec<&str> = (0..arity).map(var).collect();
    let cols = names.join(", ");
    format!("?[{cols}] <- $rows :put {rel} {{{cols}}}")
}

fn facts_rows_param(rows: &BTreeSet<Tuple>) -> DataValue {
    DataValue::List(
        rows.iter()
            .map(|t| DataValue::List(t.iter().cloned().collect()))
            .collect(),
    )
}

fn entry_line(rel: &Rel, bound: &[Option<i64>]) -> String {
    let mut head_vars = Vec::new();
    let mut args = Vec::new();
    for (i, b) in bound.iter().enumerate() {
        match b {
            Some(v) => args.push(v.to_string()),
            None => {
                head_vars.push(var(i).to_string());
                args.push(var(i).to_string());
            }
        }
    }
    format!("?[{}] := {rel}[{}]", head_vars.join(", "), args.join(", "))
}

fn edb_relations(program: &Program) -> BTreeMap<Rel, usize> {
    let heads: BTreeSet<Rel> = program.rules.iter().map(|r| r.head_rel.clone()).collect();
    let mut edb = BTreeMap::new();
    for rule in &program.rules {
        for lit in &rule.body {
            if !heads.contains(&lit.rel) {
                edb.entry(lit.rel.clone())
                    .or_insert_with(|| lit.args.len());
            }
        }
    }
    edb
}

fn gen_program(rng: &mut Rng) -> (Program, Vec<(Rel, usize)>) {
    let n = rng.range(4, 12);
    let self_join = rng.chance(1, 2);
    let with_negation = rng.chance(1, 2);

    let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
    let n_edges = rng.below((n * 3) as u64) as i64 + 1;
    let edges: BTreeSet<Tuple> = (0..n_edges)
        .map(|_| {
            vec![
                DataValue::from(rng.range(0, n)),
                DataValue::from(rng.range(0, n)),
            ]
        })
        .map(Tuple::from_vec)
        .collect();
    facts.insert("edge".into(), edges);
    facts.insert(
        "node".into(),
        (0..n)
            .map(|i| vec![DataValue::from(i)])
            .map(Tuple::from_vec)
            .collect(),
    );

    let (a, b, c) = (Term::var("a"), Term::var("b"), Term::var("c"));
    let mut rules = vec![Rule::plain(
        "path",
        vec![a.clone(), b.clone()],
        vec![Literal::pos("edge", vec![a.clone(), b.clone()])],
    )];
    if self_join {
        rules.push(Rule::plain(
            "path",
            vec![a.clone(), c.clone()],
            vec![
                Literal::pos("path", vec![a.clone(), b.clone()]),
                Literal::pos("path", vec![b.clone(), c.clone()]),
            ],
        ));
    } else {
        rules.push(Rule::plain(
            "path",
            vec![a.clone(), c.clone()],
            vec![
                Literal::pos("edge", vec![a.clone(), b.clone()]),
                Literal::pos("path", vec![b.clone(), c.clone()]),
            ],
        ));
    }
    let mut entries = vec![("path".into(), 2usize)];
    if with_negation {
        rules.push(Rule::plain(
            "unreachable",
            vec![a.clone(), b.clone()],
            vec![
                Literal::pos("node", vec![a.clone()]),
                Literal::pos("node", vec![b.clone()]),
                Literal::neg("path", vec![a.clone(), b.clone()]),
            ],
        ));
        entries.push(("unreachable".into(), 2));
    }
    (Program::untimed(rules, vec![], facts), entries)
}

const TRANSITIVE_CLOSURE: &str = "path[x, y] := *edge[x, y]
         path[x, z] := path[x, y], *edge[y, z]
         ?[x, y] := path[x, y]";

/// The MATCH case: a real recursive query (transitive closure) agrees
/// between the production evaluator and the oracle.
#[test]
fn verify_matches_on_a_real_recursive_query() {
    let db = seeded_db();
    let outcome = verify_script(
        &db,
        TRANSITIVE_CLOSURE,
        no_params(),
        ScriptOptions::default(),
    )
    .expect("verify_script runs");
    match outcome {
        VerifyOutcome::Match { row_count } => {
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
    let outcome = verify_program_sabotaged(
        &db,
        TRANSITIVE_CLOSURE,
        no_params(),
        ScriptOptions::default(),
        &|facts| {
            facts
                .get_mut("edge")
                .expect("edge was scanned")
                .remove(&Tuple::from_vec(vec![
                    DataValue::from(3i64),
                    DataValue::from(4i64),
                ]));
        },
    )
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
            let dropped: Tuple =
                Tuple::from_vec(vec![DataValue::from(3i64), DataValue::from(4i64)]);
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
    let outcome = verify_script(
        &db,
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
        | VerifyOutcome::OracleRefused { .. }) => {
            panic!("expected Unsupported(Predicate), got {other:?}")
        }
    }
}

/// Ruling (b): production `::verify` is IndexOpNotLanded. The differential
/// Match that the directive once returned now lives in trials
/// [`verify_script`]. Also assert the production door stays honest.
#[test]
fn verify_directive_runs_through_run_script() {
    let db = seeded_db();
    let refused = db.run_script(
        "::verify { path[x, y] := *edge[x, y]
         path[x, z] := path[x, y], *edge[y, z]
         ?[x, y] := path[x, y] }",
        no_params(),
    );
    assert!(
        refused.is_err(),
        "production ::verify must stay IndexOpNotLanded, got {refused:?}"
    );

    let outcome = verify_script(
        &db,
        "path[x, y] := *edge[x, y]
         path[x, z] := path[x, y], *edge[y, z]
         ?[x, y] := path[x, y]",
        no_params(),
        ScriptOptions::default(),
    )
    .expect("trials verify_script runs");
    match outcome {
        VerifyOutcome::Match { .. } => {}
        other => panic!("expected Match from trials differential, got {other:?}"),
    }
}

/// The directive's unsupported naming, re-homed: production `::verify`
/// refuses as not-landed; trials differential names Predicate.
#[test]
fn verify_directive_names_unsupported_constructs() {
    let db = seeded_db();
    let refused = db.run_script("::verify { ?[x, y] := *edge[x, y], y > 2 }", no_params());
    assert!(refused.is_err(), "production ::verify must stay not-landed");

    let outcome = verify_script(
        &db,
        "?[x, y] := *edge[x, y], y > 2",
        no_params(),
        ScriptOptions::default(),
    )
    .expect("trials verify_script runs");
    match outcome {
        VerifyOutcome::Unsupported {
            reason: VerifyUnsupported::Translate(TranslateUnsupported::Predicate),
        } => {}
        other => panic!("expected Unsupported(Predicate), got {other:?}"),
    }
}

/// Every accepted query in a wide, seeded, randomly generated corpus
/// returns `Match` through the trials differential.
#[test]
fn verify_matches_across_a_generated_corpus() {
    const SEEDS: u64 = 40;
    let mut failures = Vec::new();
    for seed in 0..SEEDS {
        let mut rng = Rng::new(seed);
        let (program, entries) = gen_program(&mut rng);
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = new_fjall_storage(dir.path()).expect("open fjall");
        std::mem::forget(dir);
        let db = open_engine(storage);
        for (rel, rows) in &program.facts {
            let arity = rows.iter().next().map(|t| t.len()).unwrap_or(0);
            db.run_script(&facts_create_schema(rel, arity), no_params())
                .unwrap_or_else(|e| panic!("seed {seed}: create {rel}: {e}"));
            if !rows.is_empty() {
                db.run_script(
                    &facts_put_script(rel, arity),
                    BTreeMap::from([("rows".into(), facts_rows_param(rows))]),
                )
                .unwrap_or_else(|e| panic!("seed {seed}: fact load for {rel}: {e}"));
            }
        }
        let rules_text = rules_script(&program);
        for (entry_rel, arity) in entries {
            let line = entry_line(&entry_rel, &vec![None; arity]);
            let script = format!("{rules_text}\n{line}");
            match verify_script(&db, &script, no_params(), ScriptOptions::default()) {
                Ok(VerifyOutcome::Match { .. }) => {}
                Ok(other) => failures.push(format!(
                    "seed {seed} entry {entry_rel}: expected match, got {other:?}"
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
    let outcome = verify_script(
        &db,
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

/// The refusal-corpus proof: `unstratifiable_corpus()` must never `Match`.
#[test]
fn verify_never_matches_the_unstratifiable_corpus() {
    use kyzo_oracle::unstratifiable_corpus;

    let mut failures = Vec::new();
    for (name, program) in unstratifiable_corpus() {
        if !program.fixed.is_empty() {
            continue;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = new_fjall_storage(dir.path()).expect("open fjall");
        std::mem::forget(dir);
        let db = open_engine(storage);
        for (rel, arity) in edb_relations(&program) {
            db.run_script(&facts_create_schema(&rel, arity), no_params())
                .unwrap_or_else(|e| panic!("{name}: create EDB {rel}: {e}"));
        }
        let rules_text = rules_script(&program);
        let heads: BTreeSet<Rel> = program.rules.iter().map(|r| r.head_rel.clone()).collect();
        for rel in heads {
            let arity = program
                .rules
                .iter()
                .find(|r| r.head_rel == rel)
                .expect("rel came from this program's own heads")
                .head_args
                .len();
            let line = entry_line(&rel, &vec![None; arity]);
            let script = format!("{rules_text}\n{line}");
            match verify_script(&db, &script, no_params(), ScriptOptions::default()) {
                Err(_) => {}
                Ok(VerifyOutcome::Match { .. }) => failures.push(format!(
                    "{name}/{rel}: differential silently matched an unstratifiable program"
                )),
                Ok(_) => {}
            }
        }
    }
    assert!(
        failures.is_empty(),
        "refusal-corpus verify FINDINGS:\n{}",
        failures.join("\n")
    );
}

fn hist_events() -> BTreeMap<Rel, Vec<Event>> {
    let key = Tuple::from_vec(vec![DataValue::from(1i64)]);
    let events = vec![
        Event::assert(
            key.clone(),
            Tuple::from_vec(vec![DataValue::from("a")]),
            100,
            1,
        )
        .expect("assert @100"),
        Event::assert(key, Tuple::from_vec(vec![DataValue::from("b")]), 200, 2)
            .expect("assert @200"),
    ];
    let mut histories = BTreeMap::new();
    histories.insert(Name::owned("hist"), events);
    histories
}

/// Two versions of the same fact: differential must agree at EACH instant.
#[test]
fn verify_matches_a_point_in_time_historical_read() {
    let dir = tempfile::tempdir().unwrap();
    let storage = new_fjall_storage(dir.path()).unwrap();
    std::mem::forget(dir);
    let db = open_engine(storage);
    db.run_script(":create hist {k: Int => v: Any}", no_params())
        .expect("create hist schema");
    db.run_script(
        "?[k, v] <- $rows :put hist {k => v} @ 100",
        BTreeMap::from([(
            "rows".into(),
            DataValue::List(vec![DataValue::List(vec![
                DataValue::from(1i64),
                DataValue::from("a"),
            ])]),
        )]),
    )
    .expect("put hist @100");
    db.run_script(
        "?[k, v] <- $rows :put hist {k => v} @ 200",
        BTreeMap::from([(
            "rows".into(),
            DataValue::List(vec![DataValue::List(vec![
                DataValue::from(1i64),
                DataValue::from("b"),
            ])]),
        )]),
    )
    .expect("put hist @200");

    let histories = hist_events();
    for (q, expect) in [
        ("?[k, v] := *hist[k, v @ 100]", 1usize),
        ("?[k, v] := *hist[k, v @ 200]", 1),
        ("?[k, v] := *hist[k, v @ 50]", 0),
    ] {
        let outcome = verify_script_with_histories(
            &db,
            q,
            no_params(),
            ScriptOptions::default(),
            histories.clone(),
        )
        .expect("verify_script_with_histories runs");
        match outcome {
            VerifyOutcome::Match { row_count } => assert_eq!(row_count, expect, "query {q}"),
            other => panic!("expected Match for {q}, got {other:?}"),
        }
    }
}

/// A negated historical read still matches (variables bound positively first).
#[test]
fn verify_matches_a_negated_historical_read() {
    let dir = tempfile::tempdir().unwrap();
    let storage = new_fjall_storage(dir.path()).unwrap();
    std::mem::forget(dir);
    let db = open_engine(storage);
    db.run_script(":create hist {k: Int => v: Any}", no_params())
        .expect("create hist schema");
    db.run_script(
        "?[k, v] <- $rows :put hist {k => v} @ 100",
        BTreeMap::from([(
            "rows".into(),
            DataValue::List(vec![DataValue::List(vec![
                DataValue::from(1i64),
                DataValue::from("a"),
            ])]),
        )]),
    )
    .expect("put hist @100");
    db.run_script(":create probe {k: Int => v: Any}", no_params())
        .expect("create probe schema");
    db.run_script(
        "?[k, v] <- $rows :put probe {k => v}",
        BTreeMap::from([(
            "rows".into(),
            DataValue::List(vec![
                DataValue::List(vec![DataValue::from(1i64), DataValue::from("a")]),
                DataValue::List(vec![DataValue::from(2i64), DataValue::from("z")]),
            ]),
        )]),
    )
    .expect("seed probe");

    let outcome = verify_script_with_histories(
        &db,
        "?[k, v] := *probe[k, v], not *hist[k, v @ 50]",
        no_params(),
        ScriptOptions::default(),
        hist_events(),
    )
    .expect("verify_script_with_histories runs");
    match outcome {
        VerifyOutcome::Match { row_count } => assert_eq!(row_count, 2),
        other => panic!("expected Match, got {other:?}"),
    }
}

/// The interval-derivation/diff boundary, named specifically.
///
/// `@spans` / `@delta` are not yet on the kyzo-model parse door (only
/// point-in-time `@ <expr>` is). The translator's typed refusal is still
/// exercised here against the IR [`ValidityClause::Spans`] shape production
/// will emit once that parse seat lands — same named outcome the historical
/// `::verify` corpus asserted.
#[test]
fn verify_refuses_a_spans_read_by_name() {
    use kyzo_model::SourceSpan;
    use kyzo_model::program::rule::InputNamedFieldRelationApplyAtom;
    use kyzo_model::value::MAX_VALIDITY_TS;

    let atom = InputAtom::NamedFieldRelation {
        inner: InputNamedFieldRelationApplyAtom {
            name: Symbol::new("hist", SourceSpan(0, 0)),
            args: BTreeMap::new(),
            validity: Some(ValidityClause::Spans {
                sys: MAX_VALIDITY_TS,
                var: Symbol::new("iv", SourceSpan(0, 0)),
            }),
            span: SourceSpan(0, 0),
        },
    };
    let mut edb = BTreeSet::new();
    let mut historical = BTreeSet::new();
    match translate_body_atom(&atom, &mut edb, &mut historical) {
        Err(TranslateUnsupported::IntervalDerivation { name }) => {
            assert!(
                name.name.contains("hist"),
                "expected the hist relation in the @spans refusal, got: {name}"
            );
        }
        other => panic!("expected Unsupported(IntervalDerivation), got {other:?}"),
    }
}

/// A starved epoch ceiling refuses on the production path (ordinary Err).
#[test]
fn verify_propagates_a_starved_epoch_ceiling_as_an_ordinary_refusal() {
    let db = seeded_db();
    let options = ScriptOptions {
        epoch_ceiling: Some(1),
        ..ScriptOptions::default()
    };
    let err = verify_script(&db, TRANSITIVE_CLOSURE, no_params(), options)
        .expect_err("a starved ceiling must refuse, not hang or silently truncate");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("epoch") || msg.contains("Epochs") || msg.contains("budget") || msg.contains("Budget") || msg.contains("Limit"),
        "expected an epoch-ceiling refusal, got: {msg}"
    );
}

/// A generous ceiling on the SAME program still matches.
#[test]
fn verify_still_matches_under_a_generous_budget() {
    let db = seeded_db();
    let options = ScriptOptions {
        epoch_ceiling: Some(1_000),
        derived_tuple_ceiling: Some(10_000),
        ..ScriptOptions::default()
    };
    let outcome = verify_script(&db, TRANSITIVE_CLOSURE, no_params(), options)
        .expect("verify_script runs");
    match outcome {
        VerifyOutcome::Match { row_count } => assert_eq!(row_count, 6),
        other => panic!("expected Match, got {other:?}"),
    }
}

/// Oracle budget defaults the derived-tuple ceiling like production.
#[test]
fn oracle_budget_defaults_derived_tuple_ceiling_like_production() {
    let defaulted = oracle_budget(&ScriptOptions::default()).expect("budget builds");
    assert_eq!(
        defaulted.derived_tuple_ceiling(),
        Some(DEFAULT_DERIVED_TUPLE_CEILING),
        "the oracle path must default the derived-tuple ceiling, never run unbounded"
    );

    let overridden = oracle_budget(&ScriptOptions {
        derived_tuple_ceiling: Some(4_242),
        ..ScriptOptions::default()
    })
    .expect("budget builds");
    assert_eq!(overridden.derived_tuple_ceiling(), Some(4_242));
}
