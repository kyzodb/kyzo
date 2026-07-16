/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Trial (issue #29): a SQLancer-class metamorphic logic-bug gauntlet for
//! KyzoScript, adapted from TLP/NoREC/PQS (Rigger & Su) to a Datalog engine
//! that has no SQL-style ternary logic to partition — see the "ternary →
//! binary" note below — and generalizing the standing fixed-corpus
//! differential (`runtime::db::tests::magic_bypass_differential`,
//! `magic_sets_demand_matches_naive_oracle_end_to_end`) to GENERATED
//! programs swept across the demand-adornment axis. Full design ruling:
//! issue #29's comment thread.
//!
//! **Oracle #1 (built here): the magic-sets NoREC-analog**, the ranked
//! highest-yield check in the ruling. `magic.rs`'s law is that the
//! demand-driven rewrite may change only *which facts get computed*, never
//! *the answer* — and its fully-free identity theorem (see `magic.rs`'s
//! module doc, issue #68) additionally promises that a fully-unbound entry
//! leaves every reachable predicate at one Muggle-cost variant, with no
//! `Input`/`Sup` machinery surviving at all. Two independent checks per
//! generated program, swept over every bound/unbound adornment of the entry
//! query:
//!
//! 1. **NoREC**: the same script run through the real engine twice — once
//!    with the magic-sets rewrite live (the default, "optimized" path) and
//!    once with `:disable_magic_rewrite true` (the "unoptimized" path,
//!    still the full compile→RA→eval pipeline, just demand-unrestricted) —
//!    must return byte-identical answers. Both are additionally checked
//!    against `laws::naive_eval`, the sealed third-party oracle, projected
//!    onto the query's bound positions. A divergence anywhere in this
//!    triangle is an engine bug, not a gauntlet bug.
//! 2. **The fully-free identity theorem, generalized**: when every
//!    argument position is unbound, the compiled magic program (inspected
//!    directly via the same `parse → normalize → stratify → magic_sets_rewrite`
//!    pipeline `Db::run_script` drives) must contain no [`MagicSymbol::Input`]
//!    or [`MagicSymbol::Sup`] variant anywhere — the symbol-count anomaly
//!    that would have caught issue #68 with no answer divergence needed.
//!
//! **Ternary → binary.** SQLancer's TLP partitions a `WHERE` predicate into
//! `TRUE`/`FALSE`/`NULL` legs because SQL's three-valued logic makes `NOT`
//! and the `NULL` leg both live concerns. KyzoScript has no NULL-as-unknown:
//! a literal holds over the finite Herbrand universe or it does not. The
//! bound/unbound adornment sweep is this oracle's one-leg-shorter analog —
//! partitioning the query space by which argument positions are constants
//! rather than by three-valued truth.
//!
//! **What this module deliberately does not render.** The generator never
//! emits aggregated rule heads or [`FixedRule`]s of its own: aggregating
//! rules are exempt from the magic-sets rewrite by construction
//! (`magic.rs`'s exemption list), so they exercise a different law (answer
//! correctness under restriction, not demand), and a [`FixedRule`] here is
//! an opaque Rust closure with no KyzoScript surface to call it through (a
//! real fixed rule needs a *named, registered* algorithm — `Db::register_fixed_rule`
//! plus a `<~` invocation — which a one-off test closure is not). The
//! refusal-fence test below (which renders `laws::unstratifiable_corpus()`
//! wholesale) hits this same wall on its one fixed-rule entry and skips it
//! for the identical, named reason.
//!
//! **Reuse, not recopy.** The program model, the naive oracle, and the
//! refusal corpus are `laws.rs`'s own — imported directly, never
//! re-derived. The splitmix64 generator is transcribed fresh rather than
//! importing `trials.rs`'s private harness: `trials.rs`'s `generate`/
//! `compile_for`/`differential` build and drive an abstract `Program`
//! through the `RuleBody`/`EvalProgram` seam directly (bypassing
//! `compile.rs` and `magic.rs` entirely, by that module's own admission —
//! see its module doc's "what this harness does not exercise"), which is
//! precisely the demand-rewrite gap oracle #1 exists to close; reaching
//! into `trials.rs` for its RNG would also mean widening that already
//! ~3000-line file's visibility while issue #30's determinism campaign is
//! concurrently in flight against it — unnecessary shared-file contention
//! for a primitive (splitmix64) this codebase already transcribes
//! independently in `storage/sim.rs` and `trials.rs` alike.

#![cfg(test)]

use std::collections::{BTreeMap, BTreeSet};

use crate::data::program::MagicSymbol;
use crate::data::value::DataValue;
use crate::data::value::Tuple;
use crate::fixed_rule::{CancelFlag, NamedRows};
use crate::parse::{Script, parse_script};
use crate::query::laws::{
    Literal, Program, Rel, Rule, Term, check_stratifiable, check_wellformed, naive_eval,
};
use crate::query::normalize::{SessionNormalizer, SessionView};
use crate::runtime::current_validity;
use crate::runtime::db::{Db, ScriptOptions, SessionTx};
use crate::storage::Storage;
use crate::storage::sim::SimStorage;

// ════════════════════════════════════════════════════════════════════════
// Seeded RNG — splitmix64, transcribed (the same primitive as
// `storage/sim.rs`'s `SimRng` and `query/trials.rs`'s `Rng`; a third
// independent transcription of an already-established codebase idiom, not
// a new one).
// ════════════════════════════════════════════════════════════════════════

pub(crate) struct Rng {
    state: u64,
}

impl Rng {
    pub(crate) fn new(seed: u64) -> Self {
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

fn no_params() -> BTreeMap<String, DataValue> {
    BTreeMap::new()
}

/// Result rows as sorted `i64` vectors, for order-independent comparison —
/// every value this gauntlet mints or projects is an integer.
fn int_rows(nr: &NamedRows) -> Vec<Vec<i64>> {
    let mut out: Vec<Vec<i64>> = nr
        .rows
        .iter()
        .map(|r| {
            r.iter()
                .map(|v| v.get_int().expect("gauntlet rows are ints"))
                .collect()
        })
        .collect();
    out.sort();
    out
}

// ════════════════════════════════════════════════════════════════════════
// KyzoScript rendering: `laws::Program` → source text. Does not exist
// anywhere else in the tree — the fixed-corpus differentials it
// generalizes hand-write their scripts one program at a time.
// ════════════════════════════════════════════════════════════════════════

/// Position-indexed variable names, matching KyzoScript's own
/// lowercase-letter convention (`?[a, b] := path[a, b]`, as seen throughout
/// `runtime/db.rs`'s public-surface tests).
fn var(i: usize) -> &'static str {
    const NAMES: [&str; 6] = ["a", "b", "c", "d", "e", "f"];
    NAMES[i]
}

fn term_text(t: &Term) -> String {
    match t {
        Term::Var(v) => (*v).to_string(),
        Term::Const(dv) => dv
            .get_int()
            .expect("gauntlet only mints int constants")
            .to_string(),
    }
}

/// A relation is IDB (no `*` sigil) iff some rule in the same program heads
/// it — the real semantic test, not "is it in `Program::facts`" (the
/// refusal corpus's external EDB relations, e.g. `d`, `move`, are never in
/// `facts` at all; they are still stored reads at the KyzoScript surface).
fn is_idb(program: &Program, rel: Rel) -> bool {
    program.rules.iter().any(|r| r.head_rel == rel)
}

fn literal_text(program: &Program, lit: &Literal) -> String {
    let sigil = if is_idb(program, lit.rel) { "" } else { "*" };
    let args: Vec<String> = lit.args.iter().map(term_text).collect();
    format!(
        "{}{sigil}{}[{}]",
        if lit.negated { "not " } else { "" },
        lit.rel,
        args.join(", ")
    )
}

/// A rule's head, aggregation included (`rel(x)` vs `rel(count(x))`) —
/// needed only by the refusal-fence renderer below, since this module's own
/// generator never mints an aggregated head (see the module doc).
fn rule_text(program: &Program, rule: &Rule) -> String {
    let head_args: Vec<String> = rule
        .head_args
        .iter()
        .zip(rule.aggr.iter())
        .map(|(t, a)| {
            let base = term_text(t);
            match a {
                Some((aggr, _args)) => format!("{}({base})", aggr.name),
                None => base,
            }
        })
        .collect();
    let body: Vec<String> = rule.body.iter().map(|l| literal_text(program, l)).collect();
    format!(
        "{}[{}] := {}",
        rule.head_rel,
        head_args.join(", "),
        body.join(", ")
    )
}

pub(crate) fn rules_script(program: &Program) -> String {
    program
        .rules
        .iter()
        .map(|r| rule_text(program, r))
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn facts_script(rel: Rel, arity: usize, rows: &BTreeSet<Tuple>) -> String {
    let names: Vec<&str> = (0..arity).map(var).collect();
    let body: Vec<String> = rows
        .iter()
        .map(|t| {
            format!(
                "[{}]",
                t.iter()
                    .map(|v| v.get_int().expect("gauntlet facts are ints").to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
        .collect();
    format!(
        "?[{n}] <- [{b}] :create {rel} {{{n}}}",
        n = names.join(", "),
        b = body.join(", ")
    )
}

/// The query entry line for one bound/unbound adornment pattern: `None`
/// leaves a position free (projected out, KyzoScript-variable), `Some(v)`
/// binds it to the constant `v` (not projected) — the shape of
/// `?[d] := path[1, d]` in `runtime/db.rs`'s own end-to-end theorem test,
/// generalized to any arity and any subset of bound positions.
pub(crate) fn entry_line(rel: Rel, bound: &[Option<i64>]) -> String {
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

/// The oracle's own relation, filtered to the bound positions and projected
/// onto the free ones — the reference answer every rendered adornment
/// pattern must match.
fn project(rows: &BTreeSet<Tuple>, bound: &[Option<i64>]) -> Vec<Vec<i64>> {
    let mut out: Vec<Vec<i64>> = rows
        .iter()
        .filter(|t| {
            bound
                .iter()
                .enumerate()
                .all(|(i, b)| b.is_none_or(|v| t[i].get_int() == Some(v)))
        })
        .map(|t| {
            bound
                .iter()
                .enumerate()
                .filter(|(_, b)| b.is_none())
                .map(|(i, _)| t[i].get_int().expect("gauntlet rows are ints"))
                .collect()
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

// ════════════════════════════════════════════════════════════════════════
// The compiled-plan seam: proves the rewrite actually fired, and (for a
// fully-unbound entry) that it left no `Input`/`Sup` machinery behind.
// Reimplemented against the same `pub(crate)` seams
// `runtime::db::tests::compiled_magic_symbols` drives (parse → normalize →
// stratify → magic_sets_rewrite), rather than reaching into `db.rs`'s test
// module (private to it) — zero edits to `db.rs`.
// ════════════════════════════════════════════════════════════════════════

fn compiled_magic_symbols<S: Storage>(db: &Db<S>, script: &str) -> Vec<MagicSymbol> {
    let cur_vld = current_validity().expect("current validity");
    let fixed = db.fixed_rules();
    let prog = match parse_script(script, &no_params(), &fixed, cur_vld)
        .expect("gauntlet script parses")
    {
        Script::Single(p) => *p,
        Script::Imperative(_) | Script::Sys(_) => panic!("gauntlet scripts are always a single query"),
    };
    let tx = SessionTx::new_read(
        db.storage.read_tx().expect("read tx"),
        ScriptOptions::default(),
    );
    let view = SessionView {
        store: &tx.store,
        temp: &tx.temp,
    };
    let mut normalizer = SessionNormalizer::new(view, CancelFlag::default());
    let (nf, _) = prog
        .into_normalized_program(&mut normalizer)
        .expect("gauntlet program normalizes");
    let (strat, _lifetimes) = nf
        .into_stratified_program()
        .expect("gauntlet program is stratifiable by construction");
    let magic = strat
        .magic_sets_rewrite(&view)
        .expect("magic-sets rewrite runs");
    magic
        .into_strata()
        .into_iter()
        .flat_map(|m| m.prog.into_keys())
        .collect()
}

// ════════════════════════════════════════════════════════════════════════
// The generator: transitive closure (linear or self-join, matching the
// points-to self-join shape issue #68 broke on) plus an optional
// negation-over-recursion reader — the two demand-rewrite-relevant shapes,
// swept across graph size and edge density.
// ════════════════════════════════════════════════════════════════════════

pub(crate) fn gen_program(rng: &mut Rng) -> (Program, Vec<(Rel, usize)>) {
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
    facts.insert("edge", edges);
    facts.insert(
        "node",
        (0..n)
            .map(|i| vec![DataValue::from(i)])
            .map(Tuple::from_vec)
            .collect(),
    );

    let (a, b, c) = (Term::Var("a"), Term::Var("b"), Term::Var("c"));
    let mut rules = vec![Rule::plain(
        "path",
        vec![a.clone(), b.clone()],
        vec![Literal::pos("edge", vec![a.clone(), b.clone()])],
    )];
    if self_join {
        // The points-to self-join shape: `path` occurs twice in its own
        // recursive rule's body, exactly the pattern that spuriously
        // adorned under a fully-unbound entry pre-#68's fix.
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
    let mut entries = vec![("path", 2)];
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
        entries.push(("unreachable", 2));
    }
    (Program::untimed(rules, vec![], facts), entries)
}

/// Every bound/unbound adornment this gauntlet sweeps for one arity: fully
/// free, and each single position bound to a value pulled from a REAL
/// oracle-derived fact (so the bound patterns are non-vacuous, not just
/// trivially empty). Two-argument bound-both patterns are left out: they
/// need a zero-projection query head, and no corpus test in this tree
/// exercises `?[]` KyzoScript syntax to confirm it parses — an unnecessary
/// risk for a pattern the theorem (which governs fully-free queries) does
/// not need.
fn adornment_patterns(arity: usize, sample: &[i64]) -> Vec<Vec<Option<i64>>> {
    let mut patterns = vec![vec![None; arity]];
    for i in 0..arity {
        let mut bound = vec![None; arity];
        bound[i] = Some(sample[i]);
        patterns.push(bound);
    }
    patterns
}

fn run_one_seed(seed: u64) -> Result<(), String> {
    let mut rng = Rng::new(seed);
    let (program, entries) = gen_program(&mut rng);
    check_wellformed(&program)
        .map_err(|e| format!("seed {seed}: generator produced an ill-formed program: {e:?}"))?;
    let oracle = naive_eval(&program)
        .map_err(|e| format!("seed {seed}: oracle refused a well-formed program: {e:?}"))?;

    let db = Db::new(SimStorage::new(seed)).map_err(|e| format!("seed {seed}: db open: {e}"))?;
    for (rel, rows) in &program.facts {
        let arity = rows.iter().next().map(|t| t.len()).unwrap_or(0);
        db.run_script(&facts_script(rel, arity, rows), no_params())
            .map_err(|e| format!("seed {seed}: fact load for {rel}: {e}"))?;
    }
    let rules_text = rules_script(&program);

    for (entry_rel, arity) in entries {
        let full = oracle.get(entry_rel).cloned().unwrap_or_default();
        let sample: Vec<i64> = full
            .iter()
            .next()
            .map(|t| {
                t.iter()
                    .map(|v| v.get_int().expect("oracle rows are ints"))
                    .collect()
            })
            .unwrap_or_else(|| vec![0; arity]);

        for bound in adornment_patterns(arity, &sample) {
            let line = entry_line(entry_rel, &bound);
            let script_on = format!("{rules_text}\n{line}");
            let script_off = format!("{script_on}\n:disable_magic_rewrite true");
            let want = project(&full, &bound);

            let got_on = int_rows(&db.run_script(&script_on, no_params()).map_err(|e| {
                format!("seed {seed}: {entry_rel} bound {bound:?}: magic-ON errored: {e}\nscript:\n{script_on}")
            })?);
            if got_on != want {
                return Err(format!(
                    "seed {seed}: {entry_rel} bound {bound:?}: MAGIC-ON mismatch — got {got_on:?}, oracle wants {want:?}\nscript:\n{script_on}"
                ));
            }

            let got_off = int_rows(&db.run_script(&script_off, no_params()).map_err(|e| {
                format!("seed {seed}: {entry_rel} bound {bound:?}: magic-OFF errored: {e}\nscript:\n{script_off}")
            })?);
            if got_off != want {
                return Err(format!(
                    "seed {seed}: {entry_rel} bound {bound:?}: MAGIC-OFF mismatch — got {got_off:?}, oracle wants {want:?}\nscript:\n{script_off}"
                ));
            }

            if bound.iter().all(Option::is_none) {
                let syms = compiled_magic_symbols(&db, &script_on);
                let leftover: Vec<String> = syms
                    .iter()
                    .filter(|s| matches!(s, MagicSymbol::Input { .. } | MagicSymbol::Sup { .. }))
                    .map(|s| format!("{s:?}"))
                    .collect();
                if !leftover.is_empty() {
                    return Err(format!(
                        "seed {seed}: {entry_rel} fully unbound left demand machinery behind \
                         (fully-free identity theorem violated): {leftover:?}\nscript:\n{script_on}"
                    ));
                }
            }
        }
    }
    Ok(())
}

/// How many seeds to sweep — the `KYZO_TRIALS_SEEDS` pattern
/// (`query/trials.rs`), under this gauntlet's own name so a CI lane can
/// tune it independently of the determinism campaign.
fn seed_count() -> u64 {
    std::env::var("KYZO_GAUNTLET_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(48)
}

fn seed_base() -> u64 {
    std::env::var("KYZO_GAUNTLET_BASE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

#[test]
fn magic_sets_norec_sweep_matches_naive_oracle_across_bound_unbound_adornment() {
    let base = seed_base();
    let count = seed_count();
    let mut failures: Vec<String> = Vec::new();
    for i in 0..count {
        let seed = Rng::new(base ^ i.wrapping_mul(0x9E37_79B9_7F4A_7C15)).next_u64();
        if let Err(f) = run_one_seed(seed) {
            failures.push(f);
        }
    }
    assert!(
        failures.is_empty(),
        "gauntlet FINDINGS ({} of {count}):\n{}",
        failures.len(),
        failures.join("\n---\n")
    );
}

// Regression pins for seeds this campaign has surfaced go here, each as a
// named test asserting `run_one_seed(SEED).is_ok()`. None to date.

#[test]
fn generator_is_seed_reproducible() {
    for seed in [1u64, 2, 42, 999, 0xDEAD_BEEF] {
        let (p1, e1) = gen_program(&mut Rng::new(seed));
        let (p2, e2) = gen_program(&mut Rng::new(seed));
        assert_eq!(
            rules_script(&p1),
            rules_script(&p2),
            "seed {seed}: rendered rules must be byte-identical"
        );
        assert_eq!(p1.facts, p2.facts, "seed {seed}: facts must be identical");
        assert_eq!(e1, e2, "seed {seed}: entries must be identical");
    }
}

/// Falsification clause (issue #29's ruling, clause 1): the sweep's own
/// equality check must not be vacuously permissive. Proves the real engine
/// matches the oracle on a real generated program, THEN proves that a
/// deliberately corrupted expectation is caught as a mismatch — the same
/// comparison `run_one_seed` relies on, exercised directly.
#[test]
fn checker_detects_a_seeded_wrong_expected_answer() {
    let mut rng = Rng::new(0xFA15E);
    let (program, entries) = gen_program(&mut rng);
    let oracle = naive_eval(&program).expect("generator produces well-formed programs");

    let db = Db::new(SimStorage::new(0xFA15E)).expect("db open");
    for (rel, rows) in &program.facts {
        let arity = rows.iter().next().map(|t| t.len()).unwrap_or(0);
        db.run_script(&facts_script(rel, arity, rows), no_params())
            .expect("facts load");
    }
    let (entry_rel, arity) = entries[0];
    let bound = vec![None; arity];
    let script = format!(
        "{}\n{}",
        rules_script(&program),
        entry_line(entry_rel, &bound)
    );
    let got = int_rows(&db.run_script(&script, no_params()).expect("query"));
    let want = project(&oracle.get(entry_rel).cloned().unwrap_or_default(), &bound);
    assert_eq!(
        got, want,
        "sanity: real engine matches the oracle before any corruption"
    );

    assert!(
        !want.is_empty(),
        "fixture must be non-vacuous for this falsification to mean anything"
    );
    let mut corrupted = want.clone();
    corrupted.pop();
    assert_ne!(
        got, corrupted,
        "the equality check the sweep relies on must distinguish a corrupted expectation \
         from the real answer"
    );
}

// ════════════════════════════════════════════════════════════════════════
// The refusal fence: `laws::unstratifiable_corpus()`, rendered wholesale
// and run through the real engine. Every entry must still be refused —
// reused directly, not re-derived (issue #29's ruling).
// ════════════════════════════════════════════════════════════════════════

/// External (EDB) relations a corpus program reads but never heads,
/// with arity inferred from how they are called — corpus programs carry no
/// `facts` for these (they exist purely to be rejected before evaluation),
/// so the render step must still `:create` them (with zero rows) or the
/// real engine would refuse for "unknown relation," masking the
/// stratification refusal this fence exists to prove.
pub(crate) fn edb_relations(program: &Program) -> BTreeMap<Rel, usize> {
    let heads: BTreeSet<Rel> = program.rules.iter().map(|r| r.head_rel).collect();
    let mut edb = BTreeMap::new();
    for rule in &program.rules {
        for lit in &rule.body {
            if !heads.contains(lit.rel) {
                edb.entry(lit.rel).or_insert_with(|| lit.args.len());
            }
        }
    }
    edb
}

/// One corpus program: renders it, `:create`s its external EDB relations
/// empty, then queries every one of its own heads fully unbound — refusal
/// at ANY of them is the real engine agreeing with the oracle (a corpus
/// program's rejected relation is not always the same one its every rule
/// heads, so trying them all is the robust form of "stays refused," not a
/// single guessed entry point).
///
/// The one entry using a [`crate::query::laws::FixedRule`] is skipped: it
/// models an opaque Rust closure, and KyzoScript has no syntax to invoke an
/// unregistered algorithm — the same named boundary the module doc states
/// for this generator's own scope. `laws.rs`'s own stratification tests
/// (`fixed_rules_sit_on_stratum_boundaries`) already cover that entry's
/// substance.
fn corpus_case_is_refused(name: &str, program: &Program) -> Result<(), String> {
    if !program.fixed.is_empty() {
        return Ok(());
    }
    if check_stratifiable(program).is_ok() {
        return Err(format!(
            "{name}: the oracle itself accepts this program as stratifiable — it no longer \
             represents a rejection, so this corpus entry needs attention upstream in laws.rs, \
             not here"
        ));
    }
    let db = Db::new(SimStorage::new(0xC09A)).map_err(|e| format!("{name}: db open: {e}"))?;
    for (rel, arity) in edb_relations(program) {
        db.run_script(&facts_script(rel, arity, &BTreeSet::new()), no_params())
            .map_err(|e| format!("{name}: create EDB {rel}: {e}"))?;
    }
    let rules_text = rules_script(program);
    let heads: BTreeSet<Rel> = program.rules.iter().map(|r| r.head_rel).collect();
    let any_refused = heads.iter().any(|rel| {
        let arity = program
            .rules
            .iter()
            .find(|r| r.head_rel == *rel)
            .expect("rel came from this program's own heads")
            .head_args
            .len();
        let script = format!("{rules_text}\n{}", entry_line(rel, &vec![None; arity]));
        db.run_script(&script, no_params()).is_err()
    });
    if any_refused {
        Ok(())
    } else {
        Err(format!(
            "{name}: the real engine accepted every one of this program's own heads, \
             but the oracle refuses it as unstratifiable"
        ))
    }
}

#[test]
fn unstratifiable_corpus_stays_refused_through_the_real_engine() {
    use crate::query::laws::unstratifiable_corpus;

    let mut failures = Vec::new();
    for (name, program) in unstratifiable_corpus() {
        if let Err(e) = corpus_case_is_refused(name, &program) {
            failures.push(e);
        }
    }
    assert!(
        failures.is_empty(),
        "refusal fence broke:\n{}",
        failures.join("\n---\n")
    );
}
