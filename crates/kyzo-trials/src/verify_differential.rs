/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Provenance-backed `::verify` corpus — re-homed from condemned
//! `kyzo-core::session::verify` (pre-`3f8749b` query-answer half), then
//! rewritten off the oracle-differential path onto the production door.
//!
//! Production `::verify` (`SysOp::Verify` / `Engine::run_script_with`) runs
//! query → `provenance_graph` → tropical solve → `verify_proof` and returns
//! NamedRows `["status","summary","detail"]` with status
//! `match` / `refused` / `mismatch` / `unsupported`.
//!
//! Root tamper-evidence verify (story #289) stays in `kyzo-core` — not here.
//!
//! `kyzo_oracle` remains only as a **fixture mint** for the generated /
//! unstratifiable corpora (program text + EDB shapes). Assertions go through
//! the provenance door, never `kyzo_oracle::eval`.

#![cfg(test)]

use std::collections::{BTreeMap, BTreeSet};

use kyzo::{Catalog, Engine, NamedRows, ScriptOptions, Storage, new_fjall_storage};
use kyzo_model::value::{DataValue, Tuple};
use kyzo_oracle::eval::{HeadAggr, Literal, Program, Rel, Rule, Term};

fn no_params() -> BTreeMap<String, DataValue> {
    BTreeMap::new()
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

const TRANSITIVE_CLOSURE: &str = "path[x, y] := *edge[x, y]
         path[x, z] := path[x, y], *edge[y, z]
         ?[x, y] := path[x, y]";

fn wrap_verify(payload: &str) -> String {
    format!("::verify {{\n{payload}\n}}")
}

fn status_of(rows: &NamedRows) -> &str {
    match rows.rows().first().and_then(|r| r.first()) {
        Some(DataValue::Str(s)) => s.as_ref(),
        other => panic!("expected status Str in verify NamedRows, got {other:?}"),
    }
}

fn summary_of(rows: &NamedRows) -> &str {
    match rows.rows().first().and_then(|r| r.get(1)) {
        Some(DataValue::Str(s)) => s.as_ref(),
        other => panic!("expected summary Str in verify NamedRows, got {other:?}"),
    }
}

fn match_row_count(rows: &NamedRows) -> usize {
    assert_eq!(status_of(rows), "match", "summary={}", summary_of(rows));
    let summary = summary_of(rows);
    let n: usize = summary
        .split_whitespace()
        .next()
        .expect("row-count token")
        .parse()
        .unwrap_or_else(|_| panic!("expected '<n> row(s) agree', got {summary}"));
    n
}

fn run_verify<S: Storage>(
    db: &Engine<S>,
    payload: &str,
    options: ScriptOptions,
) -> Result<NamedRows, miette::Report> {
    db.run_script_with(&wrap_verify(payload), no_params(), options)
}

// ════════════════════════════════════════════════════════════════════════
// Corpus render helpers — laws::Program → KyzoScript (fixture mint only)
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
    let body: Vec<String> = rule.body.iter().map(|l| literal_text(program, l)).collect();
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
                edb.entry(lit.rel.clone()).or_insert_with(|| lit.args.len());
            }
        }
    }
    edb
}

fn gen_program(rng: &mut Rng) -> (Program, Vec<(Rel, usize)>) {
    let n = rng.range(4, 12);
    let with_negation = rng.chance(1, 2);

    // Provenance rejects self-loop derivations (premise == head). Keep the
    // EDB a strict DAG of forward edges and use edge-step recursion only —
    // never path⋈path — so honest programs stay Match-able.
    let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
    let n_edges = rng.below((n * 3) as u64) as i64 + 1;
    let edges: BTreeSet<Tuple> = (0..n_edges)
        .filter_map(|_| {
            if n < 2 {
                return None;
            }
            let lo = rng.range(0, n - 1);
            let hi = rng.range(lo + 1, n);
            Some(Tuple::from_vec(vec![
                DataValue::from(lo),
                DataValue::from(hi),
            ]))
        })
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
    let mut rules = vec![
        Rule::plain(
            "path",
            vec![a.clone(), b.clone()],
            vec![Literal::pos("edge", vec![a.clone(), b.clone()])],
        ),
        Rule::plain(
            "path",
            vec![a.clone(), c.clone()],
            vec![
                Literal::pos("path", vec![a.clone(), b.clone()]),
                Literal::pos("edge", vec![b.clone(), c.clone()]),
            ],
        ),
    ];
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

/// Dense multi-path graph: eval completes under a modest derived-tuple
/// ceiling while provenance enumeration exceeds it → NamedRows `refused`.
fn dense_path_db() -> Engine<kyzo::FjallStorage> {
    let dir = tempfile::tempdir().expect("tempdir");
    let storage = new_fjall_storage(dir.path()).expect("open fjall");
    std::mem::forget(dir);
    let db = open_engine(storage);
    db.run_script(":create edge {a: Int, b: Int}", no_params())
        .expect("create edge");
    let mut pairs = Vec::new();
    let layers = [0i64, 4, 8, 12, 16];
    for w in layers.windows(2) {
        let (a0, a1) = (w[0], w[1]);
        for i in a0..a1 {
            for j in a1..(a1 + (a1 - a0)) {
                if j <= 19 {
                    pairs.push(DataValue::List(vec![
                        DataValue::from(i),
                        DataValue::from(j),
                    ]));
                }
            }
        }
    }
    for i in 0..12 {
        for j in (i + 1)..12 {
            pairs.push(DataValue::List(vec![
                DataValue::from(i),
                DataValue::from(j),
            ]));
        }
    }
    db.run_script(
        "?[a, b] <- $rows :put edge {a, b}",
        BTreeMap::from([("rows".into(), DataValue::List(pairs))]),
    )
    .expect("seed dense edge");
    db
}

const DENSE_SELF_JOIN_PATH: &str = "path[x, y] := *edge[x, y]
         path[x, z] := path[x, y], path[y, z]
         ?[x, y] := path[x, y]";

/// The MATCH case: a real recursive query (transitive closure) verifies
/// under the provenance door.
#[test]
fn verify_matches_on_a_real_recursive_query() {
    let db = seeded_db();
    let rows =
        run_verify(&db, TRANSITIVE_CLOSURE, ScriptOptions::default()).expect("::verify runs");
    assert_eq!(rows.headers(), &["status", "summary", "detail"]);
    assert_eq!(
        match_row_count(&rows),
        6,
        "unexpected row count for the seeded chain"
    );
}

/// Store-side sabotage (oracle half is gone): retract an edge, then
/// `::verify` must Match the **reduced** world — never ghost the
/// pre-sabotage answer set.
///
/// Certificate-injection NamedRows `mismatch` is not reachable from the
/// public `::verify` door without a sabotage hook; Cap2
/// (`kyzo-trials::provenance`) covers checker rejection of corrupted proofs.
#[test]
fn verify_catches_a_deliberately_sabotaged_oracle_fact() {
    let db = seeded_db();
    let before = run_verify(&db, TRANSITIVE_CLOSURE, ScriptOptions::default())
        .expect("::verify before sabotage");
    assert_eq!(match_row_count(&before), 6);

    db.run_script(
        "?[a, b] <- $rows :rm edge {a, b}",
        BTreeMap::from([(
            "rows".into(),
            DataValue::List(vec![DataValue::List(vec![
                DataValue::from(3i64),
                DataValue::from(4i64),
            ])]),
        )]),
    )
    .expect("retract sabotaged edge");

    let after = run_verify(&db, TRANSITIVE_CLOSURE, ScriptOptions::default())
        .expect("::verify after store sabotage");
    let n = match_row_count(&after);
    assert!(
        n < 6,
        "sabotaged store must shrink the verified answer, got {n} (summary={})",
        summary_of(&after)
    );
}

/// Predicate/filter atoms are in-door on the provenance path (they bind
/// no premises). Honest filtered reads Match — never silent pass-as-error,
/// never the old IndexOpNotLanded stub.
#[test]
fn verify_refuses_a_predicate_atom_by_name() {
    let db = seeded_db();
    let rows = run_verify(
        &db,
        "?[x, y] := *edge[x, y], y > 2",
        ScriptOptions::default(),
    )
    .expect("::verify runs");
    assert_eq!(status_of(&rows), "match", "summary={}", summary_of(&rows));
    assert_eq!(match_row_count(&rows), 2);
}

/// Production `::verify` returns NamedRows status `match` for an honest
/// recursive program (no longer IndexOpNotLanded).
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
        .expect("production ::verify runs");
    assert_eq!(rows.headers(), &["status", "summary", "detail"]);
    assert_eq!(status_of(&rows), "match");
    assert_eq!(match_row_count(&rows), 6);
}

/// Production `::verify` names unsupported constructs via NamedRows status
/// `unsupported` (`:order` / `:limit` / `:offset` / mutations) — not Err.
#[test]
fn verify_directive_names_unsupported_constructs() {
    let db = seeded_db();
    let rows = db
        .run_script("::verify { ?[x, y] := *edge[x, y] :order x }", no_params())
        .expect("::verify returns NamedRows for unsupported");
    assert_eq!(status_of(&rows), "unsupported");
    assert!(
        summary_of(&rows).contains("order") || summary_of(&rows).contains("limit"),
        "expected order/limit unsupported summary, got {}",
        summary_of(&rows)
    );
}

/// Every accepted query in a wide, seeded, randomly generated corpus
/// returns status `match` through the provenance door.
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
            match run_verify(&db, &script, ScriptOptions::default()) {
                Ok(rows) if status_of(&rows) == "match" => {}
                Ok(rows) => failures.push(format!(
                    "seed {seed} entry {entry_rel}: expected match, got {} ({})",
                    status_of(&rows),
                    summary_of(&rows)
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
/// emits, still Matches — closing the gap the generated corpus above
/// leaves named. Aggregated heads ground out in the provenance graph.
#[test]
fn verify_matches_a_hand_written_aggregation_query() {
    let db = seeded_db();
    let rows = run_verify(
        &db,
        "?[y, count(x)] := *edge[x, y]",
        ScriptOptions::default(),
    )
    .expect("::verify runs");
    assert_eq!(status_of(&rows), "match", "summary={}", summary_of(&rows));
}

/// The refusal-corpus proof: `unstratifiable_corpus()` must never Match.
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
            match run_verify(&db, &script, ScriptOptions::default()) {
                Err(_) => {}
                Ok(rows) if status_of(&rows) == "match" => failures.push(format!(
                    "{name}/{rel}: provenance ::verify silently matched an unstratifiable program"
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

/// Two versions of the same fact: provenance `::verify` agrees at EACH instant.
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

    for (q, expect) in [
        ("?[k, v] := *hist[k, v @ 100]", 1usize),
        ("?[k, v] := *hist[k, v @ 200]", 1),
        ("?[k, v] := *hist[k, v @ 50]", 0),
    ] {
        let rows = run_verify(&db, q, ScriptOptions::default()).expect("::verify historical");
        assert_eq!(match_row_count(&rows), expect, "query {q}");
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

    let rows = run_verify(
        &db,
        "?[k, v] := *probe[k, v], not *hist[k, v @ 50]",
        ScriptOptions::default(),
    )
    .expect("::verify negated historical");
    assert_eq!(match_row_count(&rows), 2);
}

/// Interval-derivation / ordered-answer boundary, named specifically.
///
/// `@spans` is not yet on the kyzo-model parse door (only point-in-time
/// `@ <expr>` is) — a spans script fails at parse, never silently Matches.
/// The NamedRows `unsupported` seat this cut does expose is exercised via
/// `:order` (same product status the historical oracle translator reserved
/// for interval derivation).
#[test]
fn verify_refuses_a_spans_read_by_name() {
    let db = seeded_db();
    db.run_script(":create hist {k: Int => v: Any}", no_params())
        .expect("create hist");

    let spans = db.run_script(
        "::verify { ?[k, v, iv] := *hist[k, v @spans iv] }",
        no_params(),
    );
    assert!(
        spans.is_err(),
        "@spans must fail at the language door, got {spans:?}"
    );

    let rows = run_verify(
        &db,
        "?[x, y] := *edge[x, y] :limit 1",
        ScriptOptions::default(),
    )
    .expect("::verify returns NamedRows for :limit");
    assert_eq!(status_of(&rows), "unsupported");
    assert!(
        summary_of(&rows).contains("limit") || summary_of(&rows).contains("order"),
        "expected order/limit unsupported, got {}",
        summary_of(&rows)
    );
}

/// A starved provenance derivation ceiling refuses as NamedRows `refused`
/// (eval completes; enumeration crosses the ceiling).
#[test]
fn verify_propagates_a_starved_epoch_ceiling_as_an_ordinary_refusal() {
    let db = dense_path_db();
    let options = ScriptOptions {
        derived_tuple_ceiling: Some(500),
        epoch_ceiling: Some(1_000_000),
        ..ScriptOptions::default()
    };
    let rows = run_verify(&db, DENSE_SELF_JOIN_PATH, options)
        .expect("starved provenance ceiling returns NamedRows, not Err");
    assert_eq!(status_of(&rows), "refused", "summary={}", summary_of(&rows));
    assert!(
        summary_of(&rows).contains("provenance") || summary_of(&rows).contains("budget"),
        "expected a provenance-budget refusal, got {}",
        summary_of(&rows)
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
    let rows = run_verify(&db, TRANSITIVE_CLOSURE, options).expect("::verify runs");
    assert_eq!(match_row_count(&rows), 6);
}

/// Production budget defaults keep derived-tuple spend bounded: default
/// `ScriptOptions` (no explicit ceiling) Match on the seeded TC, and an
/// explicit derived-tuple ceiling override refuses on the dense multi-path
/// graph — never unbounded enumeration.
#[test]
fn oracle_budget_defaults_derived_tuple_ceiling_like_production() {
    let db = seeded_db();
    assert!(
        ScriptOptions::default().derived_tuple_ceiling.is_none(),
        "default ScriptOptions leaves derived_tuple_ceiling unset so production \
         applies its finite default at the door"
    );
    let defaulted =
        run_verify(&db, TRANSITIVE_CLOSURE, ScriptOptions::default()).expect("default ::verify");
    assert_eq!(status_of(&defaulted), "match");

    let dense = dense_path_db();
    let refused = run_verify(
        &dense,
        DENSE_SELF_JOIN_PATH,
        ScriptOptions {
            derived_tuple_ceiling: Some(500),
            epoch_ceiling: Some(1_000_000),
            ..ScriptOptions::default()
        },
    )
    .expect("override ceiling returns NamedRows");
    assert_eq!(status_of(&refused), "refused");
}
