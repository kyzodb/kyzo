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

/// Leak-intern `s` into a genuine `&'static str`, deduplicated by content in
/// a process-wide cache. See the module docs: this exists because
/// `query/laws.rs`'s `Program` was built for `&'static` test literals, and
/// growth is bounded by the distinct name vocabulary ever verified, not by
/// call volume.
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
        self.verify_script_sabotaged(payload, params, options, &|_| {})
    }

    /// [`Self::verify_script`], with a hook applied to the oracle's scanned
    /// EDB facts just before evaluation. Production always sees the real,
    /// unsabotaged snapshot — only the oracle's copy is perturbed. The
    /// no-op hook (`&|_| {}`) is [`Self::verify_script`] itself; a
    /// corrupting hook is this module's sabotage proof (`#[cfg(test)]`
    /// below): it simulates "the oracle's snapshot adapter saw a wrong
    /// fact" and proves the comparison surfaces it as a faithful
    /// [`VerifyOutcome::Mismatch`] rather than silently agreeing.
    fn verify_script_sabotaged(
        &self,
        payload: &str,
        params: BTreeMap<String, crate::data::value::DataValue>,
        options: ScriptOptions,
        sabotage_oracle_facts: &dyn Fn(&mut BTreeMap<laws::Rel, BTreeSet<Tuple>>),
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

        let tx = SessionTx::new_read(self.storage.read_tx()?, options.clone());

        // Production: the real pipeline, on this transaction's snapshot.
        let (result, limited, head, _out_opts) = self.compile_and_eval(
            &tx.store,
            &tx.temp,
            program.clone(),
            cur_vld,
            &options,
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
                        program_text: payload.to_string(),
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
        let outcome = db
            .verify_script_sabotaged(
                TRANSITIVE_CLOSURE,
                no_params(),
                ScriptOptions::default(),
                &|facts| {
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
                },
            )
            .expect("verify_script_sabotaged runs");
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
}
