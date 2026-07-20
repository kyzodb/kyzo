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

/// Plain eval answer cardinality — independent of `::verify`. A verify
/// Match whose row count disagrees with this is a silent wrong answer.
fn eval_answer_count<S: Storage>(db: &Engine<S>, payload: &str) -> usize {
    db.run_script(payload, no_params())
        .unwrap_or_else(|e| panic!("plain eval of `{payload}` failed: {e}"))
        .rows()
        .len()
}

/// Verify Match must agree with plain eval on cardinality — checker vs
/// evaluator differential. Soft "status == match" alone cannot catch a
/// ghosted or truncated certificate that still says match.
fn assert_verify_matches_eval<S: Storage>(db: &Engine<S>, payload: &str, options: ScriptOptions) {
    let expected = eval_answer_count(db, payload);
    let rows = run_verify(db, payload, options).expect("::verify runs");
    assert_eq!(
        match_row_count(&rows),
        expected,
        "verify Match count must equal plain-eval answer size for `{payload}` \
         (summary={})",
        summary_of(&rows)
    );
}

fn run_verify<S: Storage>(
    db: &Engine<S>,
    payload: &str,
    options: ScriptOptions,
) -> Result<NamedRows, miette::Report> {
    db.run_script_with(&wrap_verify(payload), no_params(), options)
}

/// Seeded chain 1→2→3→4 transitive closure: six pairs.
const SEEDED_TC_ROWS: usize = 6;
/// After retracting edge (3,4): only 1→2→3 remains → three pairs.
const REDUCED_TC_ROWS: usize = 3;

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

/// Checker-vs-evaluator differential on transitive closure: Match count
/// must equal plain-eval answer size (six seeded pairs). A ghosted or
/// truncated certificate that still says `"match"` goes red here.
#[test]
fn verify_matches_on_a_real_recursive_query() {
    let db = seeded_db();
    assert_eq!(
        eval_answer_count(&db, TRANSITIVE_CLOSURE),
        SEEDED_TC_ROWS,
        "fixture mint drifted — seeded TC must stay six pairs"
    );
    assert_verify_matches_eval(&db, TRANSITIVE_CLOSURE, ScriptOptions::default());
}

/// Store-side sabotage: retract edge (3,4). `::verify` must Match the
/// **exact** reduced TC (three pairs), never ghost the pre-sabotage six.
/// NamedRows `"mismatch"` is the certificate-injector sibling — not this.
#[test]
fn verify_catches_a_deliberately_sabotaged_oracle_fact() {
    let db = seeded_db();
    let before = run_verify(&db, TRANSITIVE_CLOSURE, ScriptOptions::default())
        .expect("::verify before sabotage");
    assert_eq!(match_row_count(&before), SEEDED_TC_ROWS);
    assert_eq!(
        match_row_count(&before),
        eval_answer_count(&db, TRANSITIVE_CLOSURE)
    );

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

    let reduced_eval = eval_answer_count(&db, TRANSITIVE_CLOSURE);
    assert_eq!(
        reduced_eval, REDUCED_TC_ROWS,
        "retract (3,4) must leave exactly the 1→2→3 closure"
    );
    let after = run_verify(&db, TRANSITIVE_CLOSURE, ScriptOptions::default())
        .expect("::verify after store sabotage");
    assert_eq!(
        match_row_count(&after),
        REDUCED_TC_ROWS,
        "sabotaged store must Match the reduced world exactly, not ghost \
         pre-sabotage answers (summary={})",
        summary_of(&after)
    );
    assert_ne!(
        match_row_count(&after),
        SEEDED_TC_ROWS,
        "reduced Match must diverge from the pre-sabotage count"
    );
}

/// Seat 59 mutation campaign: sealed golden verifies; each structural
/// fault is rejected by production `verify_proof` with a typed
/// `BadCertificate`; NamedRows status is `"mismatch"` — never reduced
/// `"match"`. No Engine forge API.
#[test]
fn verify_mismatch_under_certificate_mutation_injector() {
    use kyzo::oracle_harness::{
        CertificateFault, golden_certificate_verifies, mismatch_named_rows_under_fault,
    };

    golden_certificate_verifies();

    for fault in [
        CertificateFault::CorruptClaimedCost,
        CertificateFault::OutOfRangeDerivation,
        CertificateFault::CorruptPremiseNode,
    ] {
        let rows = mismatch_named_rows_under_fault(fault);
        assert_eq!(rows.headers(), &["status", "summary", "detail"]);
        assert_eq!(
            status_of(&rows),
            "mismatch",
            "fault {fault:?}: summary={}",
            summary_of(&rows)
        );
        assert_ne!(
            status_of(&rows),
            "match",
            "injected corruption must not soft-green as Match"
        );
        let detail: &str = match rows.rows().first().and_then(|r| r.get(2)) {
            Some(DataValue::Str(s)) => s.as_ref(),
            other => panic!("expected detail Str, got {other:?}"),
        };
        assert!(
            detail.contains("verify_proof")
                && detail.contains("injected certificate")
                && detail.contains(&format!("{fault:?}")),
            "fault {fault:?}: detail must name the typed injected rejection, got {detail}"
        );
        assert!(
            summary_of(&rows).contains("evaluated")
                && summary_of(&rows).contains("provenance"),
            "mismatch summary must name both sets, got {}",
            summary_of(&rows)
        );
    }
}

/// Filter atoms bind no premises — still Match, but cardinality must
/// equal plain eval (two edges with `y > 2`), and must be strictly
/// smaller than the unfiltered edge relation (three).
#[test]
fn verify_matches_filtered_edges_against_eval_count() {
    let db = seeded_db();
    let filtered = "?[x, y] := *edge[x, y], y > 2";
    let unfiltered = "?[x, y] := *edge[x, y]";
    let all = eval_answer_count(&db, unfiltered);
    let want = eval_answer_count(&db, filtered);
    assert_eq!(all, 3, "seeded edge relation");
    assert_eq!(want, 2, "y > 2 keeps (2,3) and (3,4)");
    assert!(want < all, "filter must shrink the answer");
    assert_verify_matches_eval(&db, filtered, ScriptOptions::default());
}

/// Production `::verify` via `run_script` must bit-agree with the
/// `run_script_with` wrap door on status + Match count — door differential,
/// not a second happy-path Match.
#[test]
fn verify_directive_runs_through_run_script() {
    let db = seeded_db();
    let via_wrap = run_verify(&db, TRANSITIVE_CLOSURE, ScriptOptions::default())
        .expect("wrap ::verify");
    let via_directive = db
        .run_script(
            "::verify { path[x, y] := *edge[x, y]
         path[x, z] := path[x, y], *edge[y, z]
         ?[x, y] := path[x, y] }",
            no_params(),
        )
        .expect("production ::verify runs");
    assert_eq!(via_directive.headers(), &["status", "summary", "detail"]);
    assert_eq!(status_of(&via_directive), status_of(&via_wrap));
    assert_eq!(
        match_row_count(&via_directive),
        match_row_count(&via_wrap)
    );
    assert_eq!(match_row_count(&via_directive), SEEDED_TC_ROWS);
    assert_eq!(
        match_row_count(&via_directive),
        eval_answer_count(&db, TRANSITIVE_CLOSURE)
    );
}

/// `:order` is NamedRows `unsupported` naming order — never Err, never
/// silent Match. Control: the same query without `:order` still Matches
/// against eval (proves the door did not start always-returning unsupported).
#[test]
fn verify_directive_names_unsupported_constructs() {
    let db = seeded_db();
    let honest = "?[x, y] := *edge[x, y]";
    assert_verify_matches_eval(&db, honest, ScriptOptions::default());

    let rows = db
        .run_script("::verify { ?[x, y] := *edge[x, y] :order x }", no_params())
        .expect("::verify returns NamedRows for unsupported");
    assert_eq!(status_of(&rows), "unsupported");
    assert_ne!(status_of(&rows), "match");
    assert!(
        summary_of(&rows).contains("order"),
        "expected order named in unsupported summary, got {}",
        summary_of(&rows)
    );
}

/// Generated corpus: every accepted entry must Match **and** agree with
/// plain-eval cardinality. Status-only Match cannot catch a wrong count.
#[test]
fn verify_matches_across_a_generated_corpus() {
    const SEEDS: u64 = 40;
    let mut failures = Vec::new();
    let mut exercised = 0usize;
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
            let eval_n = match db.run_script(&script, no_params()) {
                Ok(r) => r.rows().len(),
                Err(e) => {
                    failures.push(format!("seed {seed} entry {entry_rel}: plain eval {e}"));
                    continue;
                }
            };
            match run_verify(&db, &script, ScriptOptions::default()) {
                Ok(rows) if status_of(&rows) == "match" => {
                    let n = match_row_count(&rows);
                    if n != eval_n {
                        failures.push(format!(
                            "seed {seed} entry {entry_rel}: verify Match {n} != eval {eval_n}"
                        ));
                    } else {
                        exercised += 1;
                    }
                }
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
        exercised > 0,
        "generated corpus exercised zero Match+eval agreements — vacuous"
    );
    assert!(
        failures.is_empty(),
        "generated-corpus verify FINDINGS ({} of {SEEDS} seeds; exercised {exercised}):\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Aggregation: three seeded targets → three groups. Match without a
/// count is soft; disagreeing with plain eval goes red.
#[test]
fn verify_matches_a_hand_written_aggregation_query() {
    let db = seeded_db();
    let q = "?[y, count(x)] := *edge[x, y]";
    assert_eq!(eval_answer_count(&db, q), 3, "one count row per target");
    assert_verify_matches_eval(&db, q, ScriptOptions::default());
}

/// Unstratifiable corpus must never Match. Anti-vacuity: at least one
/// program is exercised; silent Match is the only red we care about.
#[test]
fn verify_never_matches_the_unstratifiable_corpus() {
    use kyzo_oracle::unstratifiable_corpus;

    let mut failures = Vec::new();
    let mut exercised = 0usize;
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
            exercised += 1;
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
        exercised > 0,
        "unstratifiable corpus exercised nothing — vacuous green"
    );
    assert!(
        failures.is_empty(),
        "refusal-corpus verify FINDINGS:\n{}",
        failures.join("\n")
    );
}

/// Point-in-time reads: each instant's verify Match equals plain eval.
/// Cross-instant: @100 and @200 both size 1, but @50 is empty — a door
/// that ignored validity would ghost the later put into @50.
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
        ("?[k, v] := *hist[k, v @ 150]", 1),
        ("?[k, v] := *hist[k, v @ 50]", 0),
    ] {
        assert_eq!(
            eval_answer_count(&db, q),
            expect,
            "plain eval drift at {q}"
        );
        let rows = run_verify(&db, q, ScriptOptions::default()).expect("::verify historical");
        assert_eq!(match_row_count(&rows), expect, "query {q}");
    }
    // Ghost-future check: @50 must stay empty even though @200 exists.
    assert_eq!(
        eval_answer_count(&db, "?[k, v] := *hist[k, v @ 50]"),
        0
    );
}

/// Negated historical: `@ 50` (empty hist) keeps both probe rows; `@ 100`
/// excludes `(1,a)` → one row. A door that ignored the as-of would not
/// shrink — soft count-only on the empty-hist arm alone cannot catch that.
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

    let empty_hist = "?[k, v] := *probe[k, v], not *hist[k, v @ 50]";
    let live_hist = "?[k, v] := *probe[k, v], not *hist[k, v @ 100]";
    assert_eq!(eval_answer_count(&db, empty_hist), 2);
    assert_eq!(eval_answer_count(&db, live_hist), 1);
    assert_verify_matches_eval(&db, empty_hist, ScriptOptions::default());
    assert_verify_matches_eval(&db, live_hist, ScriptOptions::default());
    assert_ne!(
        eval_answer_count(&db, empty_hist),
        eval_answer_count(&db, live_hist),
        "as-of must change the negated answer — otherwise the arms are not a differential"
    );
}

/// `@spans` is NamedRows `unsupported` naming the relation (never silent
/// Match). `:limit` returns NamedRows `unsupported` naming limit. Control:
/// the unlimited query still Matches against eval.
#[test]
fn verify_refuses_a_spans_read_by_name() {
    let db = seeded_db();
    db.run_script(":create hist {k: Int => v: Any}", no_params())
        .expect("create hist");

    let spans = db
        .run_script(
            "::verify { ?[k, v, iv] := *hist[k, v @spans iv] }",
            no_params(),
        )
        .expect("::verify returns NamedRows for @spans");
    assert_eq!(status_of(&spans), "unsupported");
    assert_ne!(status_of(&spans), "match");
    let spans_summary = summary_of(&spans);
    assert!(
        spans_summary.contains("hist")
            && (spans_summary.contains("spans") || spans_summary.contains("interval")),
        "expected hist + spans/interval named in unsupported summary, got {spans_summary}"
    );

    let unlimited = "?[x, y] := *edge[x, y]";
    assert_verify_matches_eval(&db, unlimited, ScriptOptions::default());

    let rows = run_verify(
        &db,
        "?[x, y] := *edge[x, y] :limit 1",
        ScriptOptions::default(),
    )
    .expect("::verify returns NamedRows for :limit");
    assert_eq!(status_of(&rows), "unsupported");
    assert_ne!(status_of(&rows), "match");
    assert!(
        summary_of(&rows).contains("limit"),
        "expected limit named in unsupported summary, got {}",
        summary_of(&rows)
    );
}

/// Dense self-join under a starved derived-tuple ceiling → `refused`.
/// Same DB + same program under a generous ceiling → Match equal to eval.
/// Either arm alone is soft; the differential catches a door that always
/// refuses or always matches.
#[test]
fn verify_propagates_a_starved_epoch_ceiling_as_an_ordinary_refusal() {
    let db = dense_path_db();
    let starved = ScriptOptions {
        derived_tuple_ceiling: Some(500),
        epoch_ceiling: Some(1_000_000),
        ..ScriptOptions::default()
    };
    let generous = ScriptOptions {
        derived_tuple_ceiling: Some(10_000_000),
        epoch_ceiling: Some(1_000_000),
        ..ScriptOptions::default()
    };

    let refused = run_verify(&db, DENSE_SELF_JOIN_PATH, starved)
        .expect("starved provenance ceiling returns NamedRows, not Err");
    assert_eq!(
        status_of(&refused),
        "refused",
        "summary={}",
        summary_of(&refused)
    );
    assert_ne!(status_of(&refused), "match");
    assert!(
        summary_of(&refused).contains("provenance") || summary_of(&refused).contains("budget"),
        "expected a provenance-budget refusal, got {}",
        summary_of(&refused)
    );

    // Generous arm: eval may itself be large; verify must Match that size
    // (or refuse for a different reason — but must not soft-green as Match
    // with a wrong count). Prefer Match against eval when eval completes.
    match db.run_script_with(DENSE_SELF_JOIN_PATH, no_params(), generous.clone()) {
        Ok(eval_rows) => {
            let verified = run_verify(&db, DENSE_SELF_JOIN_PATH, generous).expect("generous verify");
            assert_eq!(status_of(&verified), "match", "summary={}", summary_of(&verified));
            assert_eq!(match_row_count(&verified), eval_rows.rows().len());
        }
        Err(_) => {
            // Eval itself refused under the generous budget — then verify
            // must not silently Match either.
            let verified = run_verify(&db, DENSE_SELF_JOIN_PATH, generous);
            if let Ok(rows) = verified {
                assert_ne!(
                    status_of(&rows),
                    "match",
                    "verify must not Match when plain eval failed"
                );
            }
        }
    }
}

/// Seeded TC under an explicit generous budget must still Match eval —
/// a budget door that always refuses would go red; a wrong count too.
#[test]
fn verify_still_matches_under_a_generous_budget() {
    let db = seeded_db();
    let options = ScriptOptions {
        epoch_ceiling: Some(1_000),
        derived_tuple_ceiling: Some(10_000),
        ..ScriptOptions::default()
    };
    assert_verify_matches_eval(&db, TRANSITIVE_CLOSURE, options);
    assert_eq!(
        eval_answer_count(&db, TRANSITIVE_CLOSURE),
        SEEDED_TC_ROWS
    );
}

/// Default options Match the seeded TC against eval; the same dense
/// program under an explicit low derived-tuple ceiling refuses — never
/// unbounded enumeration, never always-Match.
#[test]
fn oracle_budget_defaults_derived_tuple_ceiling_like_production() {
    let db = seeded_db();
    assert!(
        ScriptOptions::default().derived_tuple_ceiling.is_none(),
        "default ScriptOptions leaves derived_tuple_ceiling unset so production \
         applies its finite default at the door"
    );
    assert_verify_matches_eval(&db, TRANSITIVE_CLOSURE, ScriptOptions::default());

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
    assert_ne!(status_of(&refused), "match");
}
