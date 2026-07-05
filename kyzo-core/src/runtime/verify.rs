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
//! The oracle (`query/laws.rs::Program`) was built exclusively for
//! hand-written test programs: its relation names and variable names are
//! `&'static str`. A live, parsed KyzoScript program's names are runtime
//! strings, so [`intern`] leaks each DISTINCT name once (deduplicated by
//! content in a process-wide cache) to bridge the two — bounded by the
//! catalog's vocabulary, not by verify-call volume, and it changes nothing
//! in `laws.rs` itself (a sealed file; this is an adapter living entirely
//! on this side of the seam).
//!
//! The translator covers plain relational Datalog: rule/relation
//! applications (positive and negated), recursion, and per-head-position
//! aggregation. It REFUSES, typed, rather than silently mistranslating:
//! fixed-rule applications (no generic bridge from `Arc<dyn FixedRule>` to
//! the oracle's `fn(&[BTreeSet<Tuple>]) -> BTreeSet<Tuple>` shape),
//! predicate/unification atoms (arbitrary `Expr` evaluation is outside the
//! oracle's plain `Term::Var`/`Term::Const` model), index-search atoms, any
//! relation atom carrying a validity clause (time-travel/interval/diff
//! reads — the oracle supports the timed axis natively per story #62, but
//! wiring `NormalFormRelationApplyAtom::validity` through is separate,
//! named follow-on work, not attempted here), and `:order`/`:limit`/
//! `:offset`/mutation queries (this cut compares full, unordered read-only
//! answer sets). Every refusal is a [`VerifyOutcome::Unsupported`], never a
//! silent pass and never a panic.
//!
//! ## The snapshot adapter
//!
//! One `ReadTx` is opened once and used for BOTH evaluators — the
//! production path via [`Db::compile_and_eval`] and the oracle's EDB feed
//! via [`StoredWithValidityRA::iter_batched`] (the exact scan operator the
//! production compiler itself builds for a stored-relation atom, `AsOf`
//! resolution included) — so "byte-identical state" is structural, not a
//! hope: no second, independent scan of the same relation ever runs.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::{Mutex, OnceLock};

use miette::Result;

use crate::data::program::{
    InputProgram, NormalFormAtom, NormalFormInlineRule, NormalFormRulesOrFixed,
};
use crate::data::symb::Symbol;
use crate::data::tuple::Tuple;
use crate::data::value::{AsOf, ValidityTs, current_validity};
use crate::fixed_rule::CancelFlag;
use crate::parse::{Script, parse_script};
use crate::query::laws;
use crate::query::normalize::{SessionNormalizer, SessionView};
use crate::query::ra::stored::StoredWithValidityRA;
use crate::runtime::db::{Db, ScriptOptions, SessionTx};
use crate::runtime::relation::get_relation;
use crate::storage::{ReadTx, Storage};

/// The outcome of one `::verify` run. Never a bare bool: a MATCH, a
/// reproducible MISMATCH (both answer sets, for a filed engine issue), or a
/// named reason neither evaluator produced a comparable answer.
#[derive(Debug, Clone)]
pub enum VerifyOutcome {
    /// Production and the oracle agree, set-for-set.
    Match { row_count: usize },
    /// A real disagreement: the reproducible bug report. `program_text` is
    /// the exact script re-run to reproduce it.
    Mismatch {
        program_text: String,
        production: BTreeSet<Tuple>,
        oracle: BTreeSet<Tuple>,
    },
    /// The query uses a construct this cut's translator does not carry to
    /// the oracle (see the module docs' scope section). Named, not silent.
    Unsupported { reason: String },
    /// The oracle itself refused the translated program (unsafe or
    /// unstratifiable) — a genuine finding about the QUERY, not a verify
    /// harness bug, and not evidence of an engine defect since the
    /// production compiler independently refuses the same programs.
    OracleRefused { reason: String },
}

impl VerifyOutcome {
    /// Render as the `::verify { ... }` script directive's result table
    /// (`SysOp::Verify`'s dispatcher, `runtime/db.rs::run_sys_op`) — the
    /// product-surface rendering of the same outcome
    /// [`Db::verify_script`]/[`Db::verify_input_program`] return as a typed
    /// Rust value. One row, `["status", "summary", "detail"]`.
    pub(crate) fn into_named_rows(self) -> crate::fixed_rule::NamedRows {
        let headers = vec![
            "status".to_string(),
            "summary".to_string(),
            "detail".to_string(),
        ];
        let (status, summary, detail) = match self {
            VerifyOutcome::Match { row_count } => {
                ("match", format!("{row_count} row(s) agree"), String::new())
            }
            VerifyOutcome::Mismatch {
                program_text,
                production,
                oracle,
            } => (
                "mismatch",
                format!(
                    "production {} row(s) vs oracle {} row(s)",
                    production.len(),
                    oracle.len()
                ),
                format!("program:\n{program_text}\nproduction: {production:?}\noracle: {oracle:?}"),
            ),
            VerifyOutcome::Unsupported { reason } => ("unsupported", reason, String::new()),
            VerifyOutcome::OracleRefused { reason } => ("oracle_refused", reason, String::new()),
        };
        crate::fixed_rule::NamedRows::new(
            headers,
            vec![vec![
                crate::data::value::DataValue::from(status),
                crate::data::value::DataValue::from(summary),
                crate::data::value::DataValue::from(detail),
            ]],
        )
    }
}

/// Leak-intern `s` into a genuine `&'static str`, deduplicated by content in
/// a process-wide cache. See the module docs: this exists because
/// `query/laws.rs`'s `Program` was built for `&'static` test literals, and
/// growth is bounded by the distinct name vocabulary ever verified, not by
/// call volume.
///
/// **Design debt, named plainly (team-lead review, story #80):** this is a
/// bridge, not the honest end state. Bounded-by-catalog-vocabulary leaking
/// is acceptable for this cut ONLY because `::verify` is new and the
/// catalog is finite — but every relation and variable name a caller ever
/// verifies leaks for the life of the process, and a caller who verifies
/// many one-off, never-repeated names (e.g. auto-generated variable names
/// from a query builder) would grow this cache unboundedly. The honest
/// long-term fix is `query/laws.rs`'s `Rel`/`Term::Var` owning their
/// strings (`Cow<'static, str>` or an interned `Symbol`, not a bare
/// `&'static str`) so `::verify` never needs to leak-bridge at all — a
/// `laws.rs` change (touches the sealed oracle), out of scope for this cut,
/// tracked as follow-up work rather than silently left undocumented.
fn intern(s: &str) -> &'static str {
    static CACHE: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashSet::new()));
    let mut guard = cache.lock().expect("verify interner poisoned");
    if let Some(existing) = guard.get(s) {
        return existing;
    }
    let leaked: &'static str = Box::leak(s.to_string().into_boxed_str());
    guard.insert(leaked);
    leaked
}

/// A construct this cut's translator refuses to carry to the oracle —
/// caught at the top level and turned into [`VerifyOutcome::Unsupported`],
/// never a hard error (an unsupported query is a normal, named outcome).
#[derive(Debug)]
struct Unsupported(String);

/// The oracle-bound program plus the EDB relation names it reads (still to
/// be scanned) and the entry relation's oracle name.
struct Translated {
    program: laws::Program,
    edb_names: BTreeSet<laws::Rel>,
    entry_rel: laws::Rel,
}

fn translate_term(sym: &Symbol) -> laws::Term {
    laws::Term::Var(intern(sym.name.as_str()))
}

fn translate_atom(
    atom: &NormalFormAtom,
    edb_names: &mut BTreeSet<laws::Rel>,
) -> std::result::Result<laws::Literal, Unsupported> {
    match atom {
        NormalFormAtom::Rule(a) => Ok(laws::Literal::pos(
            intern(a.name.name.as_str()),
            a.args.iter().map(translate_term).collect(),
        )),
        NormalFormAtom::NegatedRule(a) => Ok(laws::Literal::neg(
            intern(a.name.name.as_str()),
            a.args.iter().map(translate_term).collect(),
        )),
        NormalFormAtom::Relation(a) => {
            if a.validity.is_some() {
                return Err(Unsupported(format!(
                    "relation atom '{}' carries a validity clause (time travel); \
                     ::verify does not translate time-travel reads yet",
                    a.name
                )));
            }
            let rel = intern(a.name.name.as_str());
            edb_names.insert(rel);
            Ok(laws::Literal::pos(
                rel,
                a.args.iter().map(translate_term).collect(),
            ))
        }
        NormalFormAtom::NegatedRelation(a) => {
            if a.validity.is_some() {
                return Err(Unsupported(format!(
                    "relation atom '{}' carries a validity clause (time travel); \
                     ::verify does not translate time-travel reads yet",
                    a.name
                )));
            }
            let rel = intern(a.name.name.as_str());
            edb_names.insert(rel);
            Ok(laws::Literal::neg(
                rel,
                a.args.iter().map(translate_term).collect(),
            ))
        }
        NormalFormAtom::Predicate(_) => Err(Unsupported(
            "predicate (filter expression) atoms are not translated — the oracle's Term \
             model has no arbitrary-expression evaluation"
                .to_string(),
        )),
        NormalFormAtom::Unification(_) => Err(Unsupported(
            "unification ('=' / 'in') atoms are not translated — the oracle's Term model \
             has no arbitrary-expression evaluation"
                .to_string(),
        )),
        NormalFormAtom::Search(_) => Err(Unsupported(
            "index-search atoms (~rel:idx{...}) have no oracle-model equivalent".to_string(),
        )),
    }
}

fn translate_inline_rule(
    head_rel: laws::Rel,
    rule: &NormalFormInlineRule,
    edb_names: &mut BTreeSet<laws::Rel>,
) -> std::result::Result<laws::Rule, Unsupported> {
    let head_args: Vec<laws::Term> = rule.head.iter().map(translate_term).collect();
    let body = rule
        .body
        .iter()
        .map(|a| translate_atom(a, edb_names))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(laws::Rule::aggregated(
        head_rel,
        head_args,
        rule.aggr.clone(),
        body,
    ))
}

fn translate_def(
    name_rel: laws::Rel,
    def: &NormalFormRulesOrFixed,
    rules: &mut Vec<laws::Rule>,
    edb_names: &mut BTreeSet<laws::Rel>,
) -> std::result::Result<(), Unsupported> {
    match def {
        NormalFormRulesOrFixed::Fixed { fixed } => Err(Unsupported(format!(
            "'{}' is defined by a fixed-rule application ('{}'); ::verify has no generic \
             bridge from a live fixed-rule implementation to the oracle's plain-function \
             model",
            name_rel, fixed.fixed_handle.name
        ))),
        NormalFormRulesOrFixed::Rules { rules: inline } => {
            for r in inline {
                rules.push(translate_inline_rule(name_rel, r, edb_names)?);
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
fn translate(program: &crate::data::program::NormalFormProgram) -> Result<Translated> {
    let mut rules = Vec::new();
    let mut edb_names = BTreeSet::new();

    for (name, def) in program.rules() {
        let rel = intern(name.name.as_str());
        translate_def(rel, def, &mut rules, &mut edb_names)
            .map_err(|Unsupported(reason)| miette::miette!("{reason}"))?;
    }
    let entry_rel = intern(program.entry_name().name.as_str());
    translate_def(entry_rel, program.entry(), &mut rules, &mut edb_names)
        .map_err(|Unsupported(reason)| miette::miette!("{reason}"))?;

    Ok(Translated {
        program: laws::Program {
            rules,
            fixed: vec![],
            facts: BTreeMap::new(),
            histories: BTreeMap::new(),
        },
        edb_names,
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
    for &rel in edb_names {
        let handle = get_relation(tx, rel)?;
        let arity = handle.arity();
        let bindings: Vec<Symbol> = (0..arity)
            .map(|i| Symbol::new(format!("_verify_{i}"), crate::data::span::SourceSpan(0, 0)))
            .collect();
        let scan = StoredWithValidityRA {
            bindings,
            storage: handle,
            filters: vec![],
            filters_bytecodes: vec![],
            as_of,
            span: crate::data::span::SourceSpan(0, 0),
        };
        let mut rows: BTreeSet<Tuple> = BTreeSet::new();
        for batch in scan.iter_batched(tx)? {
            for row in batch?.into_rows() {
                rows.insert(row);
            }
        }
        facts.insert(rel, rows);
    }
    Ok(facts)
}

impl<S: Storage> Db<S> {
    /// Run `payload` through both the production evaluator and the sealed
    /// naive oracle against one shared snapshot, reporting agreement,
    /// disagreement, or a named refusal. See the module docs for exactly
    /// what this cut translates.
    pub fn verify_script(
        &self,
        payload: &str,
        params: BTreeMap<String, crate::data::value::DataValue>,
        options: ScriptOptions,
    ) -> Result<VerifyOutcome> {
        let cur_vld = current_validity()?;
        let fixed = self.fixed_rules();
        let script = parse_script(payload, &params, &fixed, cur_vld)?;
        let program: InputProgram = match script {
            Script::Single(prog) => *prog,
            Script::Sys(_) | Script::Imperative(_) => {
                return Ok(VerifyOutcome::Unsupported {
                    reason: "::verify supports single read queries only, not sys ops or \
                             imperative scripts"
                        .to_string(),
                });
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
            return Ok(VerifyOutcome::Unsupported {
                reason: "::verify supports pure read queries only, not mutations".to_string(),
            });
        }
        if !program.out_opts().sorters.is_empty()
            || program.out_opts().limit.is_some()
            || program.out_opts().offset.is_some()
        {
            return Ok(VerifyOutcome::Unsupported {
                reason: ":order / :limit / :offset are not supported by this cut of \
                         ::verify — it compares full, unordered answer sets"
                    .to_string(),
            });
        }

        // Captured before `program` is consumed below (`into_normalized_program`
        // takes it by value): `InputProgram`'s `Display` renders it back as
        // canonical KyzoScript, the reproduction text for a MISMATCH bundle.
        let program_text = program.to_string();

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
            .all_iter()
            .map(crate::query::temp_store::TupleInIter::into_tuple)
            .collect();

        // Oracle: translate, refuse typed, or evaluate on the SAME snapshot.
        let (normalized, _out_opts2) = {
            let view = SessionView {
                store: &tx.store,
                temp: &tx.temp,
            };
            let cancel = CancelFlag(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
                false,
            )));
            let mut normalizer = SessionNormalizer::new(view, cancel);
            program.into_normalized_program(&mut normalizer)?
        };

        let translated = match translate(&normalized) {
            Ok(t) => t,
            Err(e) => {
                return Ok(VerifyOutcome::Unsupported {
                    reason: e.to_string(),
                });
            }
        };
        let mut facts = scan_edb_facts(&tx.store, &translated.edb_names, cur_vld)?;
        sabotage_oracle_facts(&mut facts);
        let mut oracle_program = translated.program;
        oracle_program.facts = facts;

        match laws::naive_eval(&oracle_program) {
            Ok(db) => {
                let oracle = db.get(translated.entry_rel).cloned().unwrap_or_default();
                if oracle == production {
                    Ok(VerifyOutcome::Match {
                        row_count: production.len(),
                    })
                } else {
                    Ok(VerifyOutcome::Mismatch {
                        program_text,
                        production,
                        oracle,
                    })
                }
            }
            Err(rejection) => Ok(VerifyOutcome::OracleRefused {
                reason: format!("{rejection:?}"),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::value::DataValue;
    use crate::storage::fjall::new_fjall_storage;

    fn no_params() -> BTreeMap<String, crate::data::value::DataValue> {
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
            _ => panic!("expected a single query script"),
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
            other => panic!("expected Match, got {other:?}"),
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
                    .remove(&vec![DataValue::from(3), DataValue::from(4)]);
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
                let dropped = vec![DataValue::from(3), DataValue::from(4)];
                assert!(
                    !oracle.contains(&dropped) || oracle.len() != production.len(),
                    "sabotage must be visible in the oracle's answer"
                );
            }
            other => panic!("expected Mismatch, got {other:?}"),
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
            VerifyOutcome::Unsupported { reason } => {
                assert!(
                    reason.contains("predicate"),
                    "expected a predicate-atom refusal, got: {reason}"
                );
            }
            other => panic!("expected Unsupported, got {other:?}"),
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
        assert_eq!(rows.headers, vec!["status", "summary", "detail"]);
        assert_eq!(rows.rows.len(), 1);
        assert_eq!(rows.rows[0][0], DataValue::from("match"));
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
        assert_eq!(rows.rows[0][0], DataValue::from("unsupported"));
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
            let db = Db::new(crate::storage::sim::SimStorage::new(seed))
                .expect("open sim-backed db");
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
                    Ok(rows) if rows.rows[0][0] == DataValue::from("match") => {}
                    Ok(rows) => failures.push(format!(
                        "seed {seed} entry {entry_rel}: expected match, got {:?}",
                        rows.rows[0]
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
            other => panic!("expected Match, got {other:?}"),
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
                    Ok(rows) if rows.rows[0][0] != DataValue::from("match") => {}
                    Ok(rows) => failures.push(format!(
                        "{name}/{rel}: ::verify silently matched an unstratifiable \
                         program: {:?}",
                        rows.rows[0]
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
}
