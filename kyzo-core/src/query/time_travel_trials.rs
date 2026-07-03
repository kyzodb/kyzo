/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0).
 *
 * Time-travel trials (story #3, item C.10): the README's as-of claims proven
 * through the FULL query path — compile → RA → semi-naive eval — over a real
 * `FjallStorage`, not at the operator level. Every expected value comes from
 * an in-test naive as-of reference (`naive_asof`) that is obviously correct:
 * group by the non-validity key prefix, keep the newest surviving version at
 * or before the instant, emit it iff it is an assertion. The engine's answer
 * is differenced against that reference; a disagreement is a finding.
 *
 * These are TEST-ONLY. No engine source is changed: the module reconstructs
 * the compile-then-eval harness from `query/compile.rs`'s own test module out
 * of the same `pub(crate)` surface, and adds validity-carrying histories and
 * as-of programs on top.
 *
 * Pinned boundary semantics (verified here, and traceable to
 * `data/tuple.rs::check_key_for_validity` + the `ValidityTs(Reverse(_))`
 * ordering): an as-of read AT instant T is INCLUSIVE — it returns the newest
 * stored version whose real timestamp is <= T. At the same instant, an
 * assertion beats a retraction (assert encodes to byte 0x00, retract to 0x01,
 * and the seek lands on the assertion first). Two writes with an identical
 * (key, timestamp, is_assert) triple collapse by last-write-wins, because
 * they are the same key.
 */

#![cfg(test)]

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;

use smartstring::SmartString;

use crate::data::aggr::parse_aggr;
use crate::data::expr::Expr;
use crate::data::functions::{OP_GE, OP_LE};
use crate::data::program::{
    InputRelationHandle, MagicAtom, MagicInlineRule, MagicProgram, MagicRelationApplyAtom,
    MagicRuleApplyAtom, MagicRulesOrFixed, MagicSymbol, StoreLifetimes, StratifiedMagicProgram,
};
use crate::data::relation::{ColType, ColumnDef, NullableColType, StoredRelationMetadata};
use crate::data::span::SourceSpan;
use crate::data::symb::Symbol;
use crate::data::value::{DataValue, Validity, ValidityTs};
use crate::query::compile::{
    CompiledProgram, ExecMode, NoFixedRules, bind_for_eval, stratified_magic_compile,
};
use crate::query::eval::{Budget, RowLimit, stratified_evaluate};
use crate::query::ra::{NegationOverTimeTravelError, RelAlgebra};
use crate::runtime::relation::create_relation;
use crate::storage::fjall::{FjallStorage, new_fjall_storage};
use crate::storage::{Storage, WriteTx};

// ─────────────────────────────────────────────────────────────────────────
// Plumbing (reconstructed from query/compile.rs's private test module).
// ─────────────────────────────────────────────────────────────────────────

fn sp() -> SourceSpan {
    SourceSpan(0, 0)
}
fn sym(name: &str) -> Symbol {
    Symbol::new(name, sp())
}
fn v(i: i64) -> DataValue {
    DataValue::from(i)
}
fn s(x: &str) -> DataValue {
    DataValue::from(x)
}
fn vld(ts: i64, assert: bool) -> DataValue {
    DataValue::Validity(Validity::from((ts, assert)))
}
fn vts(ts: i64) -> ValidityTs {
    ValidityTs(Reverse(ts))
}
fn muggle(rel: &str) -> MagicSymbol {
    MagicSymbol::Muggle { inner: sym(rel) }
}
fn entry_symbol() -> MagicSymbol {
    MagicSymbol::Muggle {
        inner: Symbol::prog_entry(sp()),
    }
}
fn generous_budget() -> Budget {
    Budget::new(NonZeroU32::new(10_000).expect("nonzero")).with_derived_tuple_ceiling(1_000_000)
}

fn col(name: &str, coltype: ColType) -> ColumnDef {
    ColumnDef {
        name: SmartString::from(name),
        typing: NullableColType {
            coltype,
            nullable: false,
        },
        default_gen: None,
    }
}

// Body-atom builders.
fn rule_atom(name: &str, args: &[Symbol]) -> MagicAtom {
    MagicAtom::Rule(MagicRuleApplyAtom {
        name: muggle(name),
        args: args.to_vec(),
        span: sp(),
    })
}
/// A stored-relation body atom, as-of `at` when given.
fn rel_atom_at(name: &str, args: &[Symbol], at: Option<i64>) -> MagicAtom {
    MagicAtom::Relation(MagicRelationApplyAtom {
        name: sym(name),
        args: args.to_vec(),
        valid_at: at.map(vts),
        span: sp(),
    })
}
/// A negated stored-relation body atom, as-of `at` when given.
fn neg_rel_atom_at(name: &str, args: &[Symbol], at: Option<i64>) -> MagicAtom {
    MagicAtom::NegatedRelation(MagicRelationApplyAtom {
        name: sym(name),
        args: args.to_vec(),
        valid_at: at.map(vts),
        span: sp(),
    })
}
fn binding(var: &str) -> Expr {
    Expr::Binding {
        var: sym(var),
        tuple_pos: None,
    }
}
/// `var >= k` — a lower-bound predicate `compute_bounds` recognizes.
fn pred_ge(var: &str, k: i64) -> MagicAtom {
    MagicAtom::Predicate(Expr::Apply {
        op: &OP_GE,
        args: Box::new([
            binding(var),
            Expr::Const {
                val: v(k),
                span: sp(),
            },
        ]),
        span: sp(),
    })
}
/// `var <= k` — an upper-bound predicate `compute_bounds` recognizes.
fn pred_le(var: &str, k: i64) -> MagicAtom {
    MagicAtom::Predicate(Expr::Apply {
        op: &OP_LE,
        args: Box::new([
            binding(var),
            Expr::Const {
                val: v(k),
                span: sp(),
            },
        ]),
        span: sp(),
    })
}

type HeadAggr = Option<(crate::data::aggr::Aggregation, Vec<DataValue>)>;

fn inline_rule(head: &[Symbol], aggr: Vec<HeadAggr>, body: Vec<MagicAtom>) -> MagicInlineRule {
    MagicInlineRule {
        head: head.to_vec(),
        aggr,
        body,
    }
}
fn plain_rule(head: &[Symbol], body: Vec<MagicAtom>) -> MagicInlineRule {
    inline_rule(head, vec![None; head.len()], body)
}

fn program_of(strata: Vec<Vec<(MagicSymbol, Vec<MagicInlineRule>)>>) -> StratifiedMagicProgram {
    let strata = strata
        .into_iter()
        .map(|defs| {
            let mut prog = MagicProgram::default();
            for (name, rules) in defs {
                prog.prog.insert(name, MagicRulesOrFixed::Rules { rules });
            }
            prog
        })
        .collect();
    StratifiedMagicProgram::from_execution_order(strata).expect("entry in final stratum")
}

fn immortal_lifetimes(compiled: &[CompiledProgram]) -> StoreLifetimes {
    let mut lifetimes = StoreLifetimes::default();
    let last = compiled.len().saturating_sub(1);
    for stratum in compiled {
        for name in stratum.keys() {
            lifetimes.note_use(name.clone(), last);
        }
    }
    lifetimes
}

/// Compile against a read snapshot and evaluate to the entry rows.
fn compile_and_run(db: &FjallStorage, prog: StratifiedMagicProgram) -> BTreeSet<Tuple> {
    let rtx = db.read_tx().expect("read tx");
    let compiled = stratified_magic_compile(&rtx, prog).expect("compiles");
    let lifetimes = immortal_lifetimes(&compiled);
    let program =
        bind_for_eval::<_, NoFixedRules>(&compiled, &rtx, ExecMode::Iterator, &mut |_| {
            panic!("time-travel trials have no fixed rules")
        })
        .expect("binds");
    let outcome = stratified_evaluate(
        &program,
        &lifetimes,
        RowLimit::default(),
        &generous_budget(),
        None,
    )
    .expect("evaluates");
    outcome.store.all_iter().map(|t| t.into_tuple()).collect()
}

/// Compile-and-run, but surface the compile error instead of unwrapping —
/// for pinning typed refusals.
fn compile_err(db: &FjallStorage, prog: StratifiedMagicProgram) -> miette::Report {
    let rtx = db.read_tx().expect("read tx");
    stratified_magic_compile(&rtx, prog).expect_err("expected a compile-time refusal")
}

type Tuple = Vec<DataValue>;

// ─────────────────────────────────────────────────────────────────────────
// Histories on the kernel, and the naive as-of reference.
// ─────────────────────────────────────────────────────────────────────────

/// One stored version of a fact: a key prefix (the non-validity key
/// columns), a timestamp, an assert/retract flag, and the non-key value
/// columns.
#[derive(Clone, Debug)]
struct Version {
    key: Vec<i64>,
    ts: i64,
    assert: bool,
    vals: Vec<DataValue>,
}

fn ver(key: &[i64], ts: i64, assert: bool, vals: &[DataValue]) -> Version {
    Version {
        key: key.to_vec(),
        ts,
        assert,
        vals: vals.to_vec(),
    }
}

/// Create a time-travel relation: `key_arity` integer key columns, a trailing
/// `Validity` key column, and `val_names` non-key columns; then write every
/// version. Identical (key, ts, assert) triples are the SAME stored key, so a
/// later write overwrites an earlier one exactly as the KV `put` does.
fn write_history(
    db: &FjallStorage,
    name: &str,
    key_arity: usize,
    val_names: &[&str],
    versions: &[Version],
) {
    let mut keys: Vec<ColumnDef> = (0..key_arity)
        .map(|i| col(&format!("k{i}"), ColType::Int))
        .collect();
    keys.push(col("at", ColType::Validity));
    let non_keys: Vec<ColumnDef> = val_names.iter().map(|n| col(n, ColType::Any)).collect();
    let key_bindings = keys.iter().map(|c| sym(&c.name)).collect();
    let dep_bindings = non_keys.iter().map(|c| sym(&c.name)).collect();
    let input = InputRelationHandle {
        name: sym(name),
        metadata: StoredRelationMetadata { keys, non_keys },
        key_bindings,
        dep_bindings,
        span: sp(),
    };
    let mut tx = db.write_tx().expect("write tx");
    let handle = create_relation(&mut tx, input).expect("create relation");
    for ver in versions {
        assert_eq!(ver.key.len(), key_arity, "version key arity");
        assert_eq!(ver.vals.len(), val_names.len(), "version value arity");
        let mut row: Tuple = ver.key.iter().copied().map(v).collect();
        row.push(vld(ver.ts, ver.assert));
        row.extend(ver.vals.iter().cloned());
        let key = handle.encode_key_for_store(&row, sp()).expect("encode key");
        let val = handle.encode_val_for_store(&row, sp()).expect("encode val");
        tx.put(&key, &val).expect("put row");
    }
    tx.commit().expect("commit");
}

/// Create a plain (no-validity) all-key relation and fill it with integer
/// rows — used as a join driver for the bounded as-of scan path.
fn stored_plain(db: &FjallStorage, name: &str, arity: usize, rows: &[Vec<i64>]) {
    let keys: Vec<ColumnDef> = (0..arity)
        .map(|i| col(&format!("k{i}"), ColType::Int))
        .collect();
    let key_bindings = keys.iter().map(|c| sym(&c.name)).collect();
    let input = InputRelationHandle {
        name: sym(name),
        metadata: StoredRelationMetadata {
            keys,
            non_keys: vec![],
        },
        key_bindings,
        dep_bindings: vec![],
        span: sp(),
    };
    let mut tx = db.write_tx().expect("write tx");
    let handle = create_relation(&mut tx, input).expect("create relation");
    for r in rows {
        let row: Tuple = r.iter().copied().map(v).collect();
        let key = handle.encode_key_for_store(&row, sp()).expect("encode key");
        let val = handle.encode_val_for_store(&row, sp()).expect("encode val");
        tx.put(&key, &val).expect("put row");
    }
    tx.commit().expect("commit");
}

/// The oracle. Group by key prefix; among versions at or before `at`
/// (INCLUSIVE), take the newest; an assertion emits `(key, vals)`, a
/// retraction emits nothing. `boundary_inclusive = false` is the SABOTAGED
/// form used only by the mutation tests. `assert_wins` toggles the
/// same-instant tie-break (the correct engine behaviour is `true`).
fn naive_asof_cfg(
    versions: &[Version],
    at: i64,
    boundary_inclusive: bool,
    assert_wins: bool,
) -> BTreeSet<Tuple> {
    // Collapse identical (key, ts, assert) triples: last write wins, as `put`.
    let mut collapsed: BTreeMap<(Vec<i64>, i64, bool), Vec<DataValue>> = BTreeMap::new();
    for ver in versions {
        collapsed.insert((ver.key.clone(), ver.ts, ver.assert), ver.vals.clone());
    }
    // Group surviving versions by key prefix.
    let mut by_key: BTreeMap<Vec<i64>, Vec<(i64, bool, Vec<DataValue>)>> = BTreeMap::new();
    for ((key, ts, assert), vals) in collapsed {
        by_key.entry(key).or_default().push((ts, assert, vals));
    }
    let mut out = BTreeSet::new();
    for (key, mut versions) in by_key {
        // Newest at or before `at`.
        versions.retain(|(ts, _, _)| {
            if boundary_inclusive {
                *ts <= at
            } else {
                *ts < at
            }
        });
        let Some(max_ts) = versions.iter().map(|(ts, _, _)| *ts).max() else {
            continue;
        };
        // Tie-break at the winning instant: assertion first.
        let at_max: Vec<&(i64, bool, Vec<DataValue>)> =
            versions.iter().filter(|(ts, _, _)| *ts == max_ts).collect();
        let chosen = if assert_wins {
            at_max
                .iter()
                .find(|(_, a, _)| *a)
                .or_else(|| at_max.first())
                .copied()
        } else {
            at_max
                .iter()
                .find(|(_, a, _)| !*a)
                .or_else(|| at_max.first())
                .copied()
        };
        if let Some((_, true, vals)) = chosen {
            let mut row: Tuple = key.iter().copied().map(v).collect();
            row.extend(vals.iter().cloned());
            out.insert(row);
        }
    }
    out
}

/// The correct oracle: inclusive boundary, assert-wins tie-break.
fn naive_asof(versions: &[Version], at: i64) -> BTreeSet<Tuple> {
    naive_asof_cfg(versions, at, true, true)
}

/// The distinct instants worth reading at: before the earliest, at and
/// between every stored timestamp, and after the latest.
fn interesting_instants(versions: &[Version]) -> Vec<i64> {
    let mut ts: Vec<i64> = versions.iter().map(|x| x.ts).collect();
    ts.sort_unstable();
    ts.dedup();
    let mut out = vec![];
    if let Some(&first) = ts.first() {
        out.push(first - 1);
    }
    for w in ts.windows(2) {
        out.push(w[0]); // exactly at
        out.push((w[0] + w[1]) / 2); // strictly between (timestamps are spaced)
    }
    if let Some(&last) = ts.last() {
        out.push(last); // exactly at the last
        out.push(last + 1); // after
    }
    out.sort_unstable();
    out.dedup();
    out
}

// ─────────────────────────────────────────────────────────────────────────
// As-of program builders (full compile→RA→eval path).
// ─────────────────────────────────────────────────────────────────────────

/// `?[k0..,v0..] := *rel{k0..,at,v0..} @ AT` — a bare projection scan.
fn select_asof(key_arity: usize, val_names: &[&str], at: Option<i64>) -> StratifiedMagicProgram {
    let key_syms: Vec<Symbol> = (0..key_arity).map(|i| sym(&format!("k{i}"))).collect();
    let val_syms: Vec<Symbol> = val_names.iter().map(|n| sym(n)).collect();
    let at_sym = sym("at");
    let mut args = key_syms.clone();
    args.push(at_sym);
    args.extend(val_syms.clone());
    let mut head = key_syms;
    head.extend(val_syms);
    program_of(vec![vec![(
        entry_symbol(),
        vec![plain_rule(&head, vec![rel_atom_at("hist", &args, at)])],
    )]])
}

// ═════════════════════════════════════════════════════════════════════════
// Task 1 — histories on the kernel; boundary + same-instant semantics pinned.
// ═════════════════════════════════════════════════════════════════════════

/// AT-instant reads are INCLUSIVE: a fact asserted at t=10 is invisible at
/// t=9, visible at t=10 and t=11. This is the load-bearing boundary claim.
#[test]
fn boundary_at_instant_is_inclusive() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let hist = vec![ver(&[1], 10, true, &[s("ten")])];
    write_history(&db, "hist", 1, &["val"], &hist);

    assert_eq!(
        compile_and_run(&db, select_asof(1, &["val"], Some(9))),
        naive_asof(&hist, 9)
    );
    assert!(compile_and_run(&db, select_asof(1, &["val"], Some(9))).is_empty());
    assert_eq!(
        compile_and_run(&db, select_asof(1, &["val"], Some(10))),
        BTreeSet::from([vec![v(1), s("ten")]])
    );
    assert_eq!(
        compile_and_run(&db, select_asof(1, &["val"], Some(11))),
        BTreeSet::from([vec![v(1), s("ten")]])
    );
}

/// Same instant, assert vs retract on the same key: the assertion wins (the
/// fact is visible). This pins the same-instant tie-break.
#[test]
fn same_instant_assert_beats_retract() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let hist = vec![
        ver(&[1], 10, true, &[s("present")]),
        ver(&[1], 10, false, &[s("present")]),
    ];
    write_history(&db, "hist", 1, &["val"], &hist);
    let got = compile_and_run(&db, select_asof(1, &["val"], Some(10)));
    assert_eq!(got, BTreeSet::from([vec![v(1), s("present")]]));
    assert_eq!(got, naive_asof(&hist, 10));
}

/// Same instant, same key, same assert flag, different value: the two writes
/// are the SAME stored key, so the later value overwrites — one row survives.
#[test]
fn same_instant_identical_key_last_write_wins() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let hist = vec![
        ver(&[1], 10, true, &[s("first")]),
        ver(&[1], 10, true, &[s("second")]),
    ];
    write_history(&db, "hist", 1, &["val"], &hist);
    let got = compile_and_run(&db, select_asof(1, &["val"], Some(10)));
    assert_eq!(got, BTreeSet::from([vec![v(1), s("second")]]));
    assert_eq!(got, naive_asof(&hist, 10));
}

/// A key whose entire history is retractions is never present, at any instant.
#[test]
fn whole_history_of_retractions_never_present() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let hist = vec![
        ver(&[7], 10, false, &[s("x")]),
        ver(&[7], 20, false, &[s("x")]),
        ver(&[7], 30, false, &[s("x")]),
    ];
    write_history(&db, "hist", 1, &["val"], &hist);
    for at in [5, 10, 15, 20, 25, 30, 35] {
        let got = compile_and_run(&db, select_asof(1, &["val"], Some(at)));
        assert!(
            got.is_empty(),
            "retraction-only key visible at {at}: {got:?}"
        );
        assert_eq!(got, naive_asof(&hist, at));
    }
}

/// A rich, interleaved, multi-key history differenced against the naive
/// oracle at EVERY interesting instant: before, between, exactly-at, after.
/// Keys asserted, superseded (re-asserted with a new value), retracted, and
/// re-asserted after retraction.
#[test]
fn full_history_matrix_differential() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let hist = vec![
        // key 1: assert@10 "a", supersede@30 "b", retract@50, re-assert@70 "c"
        ver(&[1], 10, true, &[s("a")]),
        ver(&[1], 30, true, &[s("b")]),
        ver(&[1], 50, false, &[s("b")]),
        ver(&[1], 70, true, &[s("c")]),
        // key 2: assert@20 "p", retract@40 (latest fact for key 2)
        ver(&[2], 20, true, &[s("p")]),
        ver(&[2], 40, false, &[s("p")]),
        // key 3: assert@60 "q" only
        ver(&[3], 60, true, &[s("q")]),
        // key 4: assert@10 and retract@10 at the SAME instant (assert wins)
        ver(&[4], 10, true, &[s("blink")]),
        ver(&[4], 10, false, &[s("blink")]),
    ];
    write_history(&db, "hist", 1, &["val"], &hist);
    for at in interesting_instants(&hist) {
        let got = compile_and_run(&db, select_asof(1, &["val"], Some(at)));
        assert_eq!(got, naive_asof(&hist, at), "mismatch at instant {at}");
    }
}

// ═════════════════════════════════════════════════════════════════════════
// Task 2 — full-path as-of differentials: recursion, join, aggregation.
// ═════════════════════════════════════════════════════════════════════════

/// Reachability over an edge relation whose edges appear and retract over
/// time. The transitive closure — computed through REAL semi-naive recursion
/// — must differ per instant, matching a naive as-of-then-close reference.
#[test]
fn tc_over_time_reachability_differs_per_instant() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    // Edges as versions on [from, to, at]; no value columns.
    let edges = vec![
        ver(&[1, 2], 10, true, &[]),  // 1→2 from t=10
        ver(&[2, 3], 20, true, &[]),  // 2→3 from t=20
        ver(&[3, 4], 30, true, &[]),  // 3→4 from t=30
        ver(&[2, 3], 40, false, &[]), // 2→3 retracted at t=40 (breaks the chain)
    ];
    write_history(&db, "edge", 2, &[], &edges);

    let (x, y, z, at) = (sym("x"), sym("y"), sym("z"), sym("at"));
    let tc_program = |instant: i64| {
        program_of(vec![
            vec![(
                muggle("path"),
                vec![
                    plain_rule(
                        &[x.clone(), y.clone()],
                        vec![rel_atom_at(
                            "edge",
                            &[x.clone(), y.clone(), at.clone()],
                            Some(instant),
                        )],
                    ),
                    plain_rule(
                        &[x.clone(), y.clone()],
                        vec![
                            rel_atom_at("edge", &[x.clone(), z.clone(), at.clone()], Some(instant)),
                            rule_atom("path", &[z.clone(), y.clone()]),
                        ],
                    ),
                ],
            )],
            vec![(
                entry_symbol(),
                vec![plain_rule(
                    &[x.clone(), y.clone()],
                    vec![rule_atom("path", &[x.clone(), y.clone()])],
                )],
            )],
        ])
    };

    for at_instant in interesting_instants(&edges) {
        let present_edges = naive_present_edges(&edges, at_instant);
        let expected = naive_transitive_closure(&present_edges);
        let got = compile_and_run(&db, tc_program(at_instant));
        assert_eq!(
            got, expected,
            "reachability mismatch at instant {at_instant}"
        );
    }
}

/// A join between two DIFFERENT relations, each read as-of the SAME instant,
/// on their shared key. `?[k,a,b] := *ra{k,at,a}@T, *rb{k,at2,b}@T`.
#[test]
fn join_two_relations_at_same_instant() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let ha = vec![
        ver(&[1], 10, true, &[s("a1")]),
        ver(&[2], 10, true, &[s("a2")]),
        ver(&[1], 30, false, &[s("a1")]), // key 1 gone from ra at t>=30
    ];
    let hb = vec![
        ver(&[1], 10, true, &[s("b1")]),
        ver(&[2], 20, true, &[s("b2")]), // key 2 appears in rb only at t>=20
    ];
    write_history(&db, "ra", 1, &["a"], &ha);
    write_history(&db, "rb", 1, &["b"], &hb);

    let (k, at1, at2, a, b) = (sym("k"), sym("at1"), sym("at2"), sym("a"), sym("b"));
    let join_program = |instant: i64| {
        program_of(vec![vec![(
            entry_symbol(),
            vec![plain_rule(
                &[k.clone(), a.clone(), b.clone()],
                vec![
                    MagicAtom::Relation(MagicRelationApplyAtom {
                        name: sym("ra"),
                        args: vec![k.clone(), at1.clone(), a.clone()],
                        valid_at: Some(vts(instant)),
                        span: sp(),
                    }),
                    MagicAtom::Relation(MagicRelationApplyAtom {
                        name: sym("rb"),
                        args: vec![k.clone(), at2.clone(), b.clone()],
                        valid_at: Some(vts(instant)),
                        span: sp(),
                    }),
                ],
            )],
        )]])
    };

    for at in [5, 10, 15, 20, 25, 30, 35] {
        let a_rows = naive_asof(&ha, at);
        let b_rows = naive_asof(&hb, at);
        // Join on the key column (index 0).
        let mut expected = BTreeSet::new();
        for ar in &a_rows {
            for br in &b_rows {
                if ar[0] == br[0] {
                    expected.insert(vec![ar[0].clone(), ar[1].clone(), br[1].clone()]);
                }
            }
        }
        let got = compile_and_run(&db, join_program(at));
        assert_eq!(got, expected, "join mismatch at instant {at}");
    }
}

/// A normal (non-meet) aggregation over an as-of read: `?[count(k0)]`. The
/// count of present keys must track the as-of population.
#[test]
fn normal_aggregation_over_asof_read() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let hist = vec![
        ver(&[1], 10, true, &[v(100)]),
        ver(&[2], 20, true, &[v(200)]),
        ver(&[3], 30, true, &[v(300)]),
        ver(&[2], 40, false, &[v(200)]),
    ];
    write_history(&db, "hist", 1, &["val"], &hist);

    let (k0, at, val) = (sym("k0"), sym("at"), sym("val"));
    let count = parse_aggr("count").expect("count exists");
    let sum = parse_aggr("sum").expect("sum exists");
    let agg_program = |instant: i64| {
        program_of(vec![vec![(
            entry_symbol(),
            vec![inline_rule(
                &[k0.clone(), val.clone()],
                vec![Some((count, vec![])), Some((sum, vec![]))],
                vec![rel_atom_at(
                    "hist",
                    &[k0.clone(), at.clone(), val.clone()],
                    Some(instant),
                )],
            )],
        )]])
    };

    for at_i in interesting_instants(&hist) {
        let present = naive_asof(&hist, at_i);
        let cnt = present.len() as i64;
        let total: i64 = present
            .iter()
            .map(|r| match &r[1] {
                DataValue::Num(_) => r[1].get_int().unwrap_or(0),
                _ => 0,
            })
            .sum();
        let got = compile_and_run(&db, agg_program(at_i));
        let expected = if cnt == 0 {
            BTreeSet::from([vec![v(0), v(0)]])
        } else {
            BTreeSet::from([vec![v(cnt), v(total)]])
        };
        assert_eq!(got, expected, "aggregation mismatch at instant {at_i}");
    }
}

/// A MEET aggregation (`min`) over an as-of read. The minimum value among
/// present keys must track the as-of population; empty population yields no
/// row (a meet fold over no rows).
#[test]
fn meet_aggregation_over_asof_read() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let hist = vec![
        ver(&[1], 10, true, &[v(50)]),
        ver(&[2], 20, true, &[v(20)]),
        ver(&[3], 30, true, &[v(80)]),
        ver(&[2], 40, false, &[v(20)]), // the minimum retracts at t>=40
    ];
    write_history(&db, "hist", 1, &["val"], &hist);

    let (k0, at, val) = (sym("k0"), sym("at"), sym("val"));
    let min = parse_aggr("min").expect("min exists");
    assert!(min.is_meet(), "min must be a meet aggregation");
    let min_program = |instant: i64| {
        program_of(vec![vec![(
            entry_symbol(),
            vec![inline_rule(
                std::slice::from_ref(&val),
                vec![Some((min, vec![]))],
                vec![rel_atom_at(
                    "hist",
                    &[k0.clone(), at.clone(), val.clone()],
                    Some(instant),
                )],
            )],
        )]])
    };

    for at_i in interesting_instants(&hist) {
        let present = naive_asof(&hist, at_i);
        let min_val = present.iter().filter_map(|r| r[1].get_int()).min();
        let got = compile_and_run(&db, min_program(at_i));
        // An UNGROUPED aggregation emits exactly one identity row even when
        // the as-of population is empty (the same reason `count` yields `0`
        // over an empty read). `min`'s identity is `null`. This is a
        // time-travel-orthogonal Cozo semantic, pinned here so the meet fold
        // over an empty as-of read is a defined value, not a silent gap.
        let expected = match min_val {
            Some(m) => BTreeSet::from([vec![v(m)]]),
            None => BTreeSet::from([vec![DataValue::Null]]),
        };
        assert_eq!(got, expected, "meet-aggregation mismatch at instant {at_i}");
    }
}

/// The FILTERED as-of scan path: an as-of scan that is the RHS of a prefix
/// join AND carries a range predicate on a post-prefix key column drives
/// `compute_bounds` → `RelationHandle::skip_scan_bounded_prefix`
/// (`ra.rs:1447`). Relation keys are `[k0, k1, at:Validity]`; a driver
/// relation binds `k0` (the join prefix), and `k1 >= lo, k1 <= hi` bounds the
/// second key column. Differenced against the naive oracle (as-of, then
/// restrict to driver keys and the k1 range) at every instant, plus one
/// hand-computed case. (Instrumented once to confirm the bounded branch
/// actually fires — the instrumentation is not part of the patch.)
#[test]
fn bounded_asof_scan_over_post_prefix_key_range() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    // Driver keys: k0 in {1, 2}; k0=3 exists in `hist` but is excluded by the
    // join, proving the join prefix and the range filter compose.
    stored_plain(&db, "drv", 1, &[vec![1], vec![2]]);
    let hist = vec![
        ver(&[1, 10], 5, true, &[s("below")]), // k1=10, below the range
        ver(&[1, 25], 5, true, &[s("b")]),     // in range
        ver(&[1, 35], 5, true, &[s("c")]),     // in range
        ver(&[1, 45], 5, true, &[s("above")]), // k1=45, above the range
        ver(&[1, 25], 50, false, &[s("b")]),   // (1,25) retracts at t>=50
        ver(&[2, 30], 5, true, &[s("e")]),     // in range, k0=2 in driver
        ver(&[3, 25], 5, true, &[s("f")]),     // in range but k0=3 not in driver
    ];
    write_history(&db, "hist", 2, &["val"], &hist);

    let (k0, k1, at, val) = (sym("k0"), sym("k1"), sym("at"), sym("val"));
    let bounded_program = |instant: i64| {
        program_of(vec![vec![(
            entry_symbol(),
            vec![plain_rule(
                &[k0.clone(), k1.clone(), val.clone()],
                vec![
                    rel_atom_at("drv", std::slice::from_ref(&k0), None),
                    rel_atom_at(
                        "hist",
                        &[k0.clone(), k1.clone(), at.clone(), val.clone()],
                        Some(instant),
                    ),
                    pred_ge("k1", 20),
                    pred_le("k1", 40),
                ],
            )],
        )]])
    };

    let driver: BTreeSet<i64> = BTreeSet::from([1, 2]);
    let oracle = |instant: i64| -> BTreeSet<Tuple> {
        naive_asof(&hist, instant)
            .into_iter()
            .filter(|row| {
                let k0 = row[0].get_int().unwrap();
                let k1 = row[1].get_int().unwrap();
                driver.contains(&k0) && (20..=40).contains(&k1)
            })
            .collect()
    };

    // Hand-computed case at t=10 (nothing retracted yet): (1,25,b), (1,35,c),
    // (2,30,e). (1,10) below range, (1,45) above, (3,25) not in driver.
    assert_eq!(
        compile_and_run(&db, bounded_program(10)),
        BTreeSet::from([
            vec![v(1), v(25), s("b")],
            vec![v(1), v(35), s("c")],
            vec![v(2), v(30), s("e")],
        ])
    );
    // After the retraction at t=50, (1,25) drops out.
    assert_eq!(
        compile_and_run(&db, bounded_program(55)),
        BTreeSet::from([vec![v(1), v(35), s("c")], vec![v(2), v(30), s("e")]])
    );
    // Full differential across every interesting instant.
    for at_i in interesting_instants(&hist) {
        let got = compile_and_run(&db, bounded_program(at_i));
        assert_eq!(
            got,
            oracle(at_i),
            "bounded as-of mismatch at instant {at_i}"
        );
    }
}

// ═════════════════════════════════════════════════════════════════════════
// Task 3 — retraction is revision, not erasure; history is addressable.
// ═════════════════════════════════════════════════════════════════════════

/// Retraction is revision: after a fact is retracted, an as-of read at an
/// EARLIER instant still returns it (previous state addressable); the current
/// read does not; and re-assertion after retraction works.
#[test]
fn retraction_is_revision_not_erasure() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let hist = vec![
        ver(&[1], 10, true, &[s("born")]),
        ver(&[1], 50, false, &[s("born")]),  // retracted at t=50
        ver(&[1], 90, true, &[s("reborn")]), // re-asserted at t=90
    ];
    write_history(&db, "hist", 1, &["val"], &hist);
    let read = |at: i64| compile_and_run(&db, select_asof(1, &["val"], Some(at)));

    // Before retraction: the original fact is still addressable.
    assert_eq!(read(30), BTreeSet::from([vec![v(1), s("born")]]));
    // Between retraction and re-assertion: absent.
    assert!(read(70).is_empty());
    // After re-assertion: the new fact.
    assert_eq!(read(100), BTreeSet::from([vec![v(1), s("reborn")]]));
    // The retracted instant is INCLUSIVE of the retraction → absent at t=50.
    assert!(read(50).is_empty());
    // Every instant agrees with the oracle.
    for at in interesting_instants(&hist) {
        assert_eq!(read(at), naive_asof(&hist, at), "revision mismatch at {at}");
    }
}

/// The history itself is enumerable: a PLAIN (non-time-travel) scan returns
/// every stored version — assertions and retractions alike — with the
/// validity column carried as data. This is the surface by which the full
/// history is queryable today.
#[test]
fn full_history_enumerable_via_plain_scan() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let hist = vec![
        ver(&[1], 10, true, &[s("a")]),
        ver(&[1], 30, false, &[s("a")]),
        ver(&[2], 20, true, &[s("b")]),
    ];
    write_history(&db, "hist", 1, &["val"], &hist);

    let (k0, at, val) = (sym("k0"), sym("at"), sym("val"));
    let prog = program_of(vec![vec![(
        entry_symbol(),
        vec![plain_rule(
            &[k0.clone(), at.clone(), val.clone()],
            vec![rel_atom_at(
                "hist",
                &[k0.clone(), at.clone(), val.clone()],
                None,
            )],
        )],
    )]]);
    let got = compile_and_run(&db, prog);
    let expected: BTreeSet<Tuple> = hist
        .iter()
        .map(|x| vec![v(x.key[0]), vld(x.ts, x.assert), x.vals[0].clone()])
        .collect();
    assert_eq!(got, expected, "plain scan must enumerate the whole history");
}

// ═════════════════════════════════════════════════════════════════════════
// Task 4 — determinism: the as-of differential is byte-identical across
// thread counts.
// ═════════════════════════════════════════════════════════════════════════

/// The full-path as-of run is byte-identical at 1, 2, 4, and 8 worker
/// threads. (The inline-rule query path is sequential; this proves the as-of
/// scan + eval carry no thread-count-dependent nondeterminism.)
#[test]
fn asof_run_is_byte_identical_across_threads() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let edges = vec![
        ver(&[1, 2], 10, true, &[]),
        ver(&[2, 3], 20, true, &[]),
        ver(&[3, 4], 30, true, &[]),
        ver(&[4, 2], 30, true, &[]),
        ver(&[2, 3], 50, false, &[]),
    ];
    write_history(&db, "edge", 2, &[], &edges);

    let (x, y, z, at) = (sym("x"), sym("y"), sym("z"), sym("at"));
    let build = || {
        program_of(vec![
            vec![(
                muggle("path"),
                vec![
                    plain_rule(
                        &[x.clone(), y.clone()],
                        vec![rel_atom_at(
                            "edge",
                            &[x.clone(), y.clone(), at.clone()],
                            Some(35),
                        )],
                    ),
                    plain_rule(
                        &[x.clone(), y.clone()],
                        vec![
                            rel_atom_at("edge", &[x.clone(), z.clone(), at.clone()], Some(35)),
                            rule_atom("path", &[z.clone(), y.clone()]),
                        ],
                    ),
                ],
            )],
            vec![(
                entry_symbol(),
                vec![plain_rule(
                    &[x.clone(), y.clone()],
                    vec![rule_atom("path", &[x.clone(), y.clone()])],
                )],
            )],
        ])
    };

    let serialize = |rows: &BTreeSet<Tuple>| format!("{rows:?}");
    let mut baseline: Option<String> = None;
    for threads in [1usize, 2, 4, 8] {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .expect("thread pool");
        let out = pool.install(|| serialize(&compile_and_run(&db, build())));
        match &baseline {
            None => baseline = Some(out),
            Some(b) => assert_eq!(b, &out, "as-of run differs at {threads} threads"),
        }
    }
}

// ═════════════════════════════════════════════════════════════════════════
// Task 5 — the refusal boundary around validity scans.
// ═════════════════════════════════════════════════════════════════════════

/// Negation over a time-travel scan is a typed refusal at COMPILE time
/// (`NegationOverTimeTravelError`), not a silent wrong answer or a mid-query
/// abort.
#[test]
fn negation_over_validity_scan_refuses() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    write_history(&db, "hist", 1, &["val"], &[ver(&[1], 10, true, &[s("a")])]);
    write_history(&db, "base", 1, &["val"], &[ver(&[1], 10, true, &[s("b")])]);

    let (k0, at, val) = (sym("k0"), sym("at"), sym("val"));
    // ?[k0] := *base{k0,at,val}@T, not *hist{k0,at,val}@T
    let prog = program_of(vec![vec![(
        entry_symbol(),
        vec![plain_rule(
            std::slice::from_ref(&k0),
            vec![
                rel_atom_at("base", &[k0.clone(), at.clone(), val.clone()], Some(20)),
                neg_rel_atom_at("hist", &[k0.clone(), at.clone(), val.clone()], Some(20)),
            ],
        )],
    )]]);
    let err = compile_err(&db, prog);
    assert!(
        err.downcast_ref::<NegationOverTimeTravelError>().is_some(),
        "expected NegationOverTimeTravelError, got: {err:?}"
    );
}

/// Fixed rules reading stored relations (validity-keyed or not) are a seam:
/// the only `StoredInputSource` in this build is `NoStoredInputs`, which
/// refuses every stored read with a typed, spanned `StoredInputUnavailable`.
/// A fixed rule consuming a validity relation is therefore a typed refusal
/// today — NOT a silent wrong answer — pending the runtime tier. The as-of
/// argument (`valid_at`) is threaded to `stored_scan_all` at both the magic
/// tier and the fixed-rule arg tier; only the concrete reader is absent.
#[test]
fn fixed_rule_stored_input_is_a_refusing_seam() {
    use crate::fixed_rule::NoStoredInputs;
    use crate::fixed_rule::StoredInputSource;

    let src = NoStoredInputs;
    // As-of read of a (would-be) validity relation refuses, typed.
    let err = src
        .stored_scan_all(&sym("hist"), Some(vts(20)))
        .err()
        .expect("NoStoredInputs must refuse an as-of stored scan");
    assert!(
        err.to_string().contains("not available to fixed rules"),
        "expected StoredInputUnavailable, got: {err:?}"
    );
    // The non-as-of read refuses identically (the seam is total).
    assert!(src.stored_scan_all(&sym("hist"), None).is_err());
    assert!(src.stored_arity(&sym("hist")).is_err());
}

/// Aggregation over a validity scan compiles and evaluates (proved by the
/// task-2 differentials). Here we additionally pin, at the RA layer, that a
/// validity scan is a first-class relation an aggregation can consume — a
/// `StoredWithValidity` operator, constructed without refusal.
#[test]
fn validity_scan_is_a_constructible_relation() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    write_history(&db, "hist", 1, &["val"], &[ver(&[1], 10, true, &[s("a")])]);
    let rtx = db.read_tx().unwrap();
    let handle = crate::runtime::relation::get_relation(&rtx, "hist").expect("relation exists");
    let ra = RelAlgebra::relation(
        vec![sym("k0"), sym("at"), sym("val")],
        handle,
        sp(),
        Some(vts(20)),
    )
    .expect("a validity scan is constructible");
    assert!(
        matches!(ra, RelAlgebra::StoredWithValidity(_)),
        "expected a StoredWithValidity operator"
    );
}

// ═════════════════════════════════════════════════════════════════════════
// Task 6 — mutation-prove the harness: a wrong reference must be CAUGHT.
// ═════════════════════════════════════════════════════════════════════════

/// Flip the oracle's boundary from inclusive to exclusive: the engine's
/// answer must then DISAGREE with the sabotaged reference at an at-instant
/// read. If they still agreed, the differential would be blind to the
/// boundary. (Positive proof that the harness is boundary-sensitive.)
#[test]
fn mutation_exclusive_boundary_is_caught() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let hist = vec![
        ver(&[1], 10, true, &[s("ten")]),
        ver(&[2], 20, true, &[s("twenty")]),
    ];
    write_history(&db, "hist", 1, &["val"], &hist);

    // At exactly t=10, the correct (inclusive) reference includes key 1; the
    // exclusive reference excludes it.
    let engine = compile_and_run(&db, select_asof(1, &["val"], Some(10)));
    let correct = naive_asof_cfg(&hist, 10, true, true);
    let sabotaged = naive_asof_cfg(&hist, 10, false, true);
    assert_eq!(
        engine, correct,
        "engine must match the correct inclusive oracle"
    );
    assert_ne!(
        engine, sabotaged,
        "a boundary-flipped oracle must disagree with the engine — else the harness is blind"
    );
}

/// Drop a retraction from the reference history: the engine (which sees the
/// retraction) must then DISAGREE with the sabotaged reference. Proves the
/// differential is sensitive to retraction semantics.
#[test]
fn mutation_dropped_retraction_is_caught() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let full = vec![
        ver(&[1], 10, true, &[s("a")]),
        ver(&[1], 30, false, &[s("a")]), // the retraction the engine will see
    ];
    write_history(&db, "hist", 1, &["val"], &full);

    let sabotaged_history: Vec<Version> = full.iter().filter(|x| x.assert).cloned().collect();

    // At t=40 (after the retraction) the engine returns nothing; a reference
    // that dropped the retraction would still return the stale assertion.
    let engine = compile_and_run(&db, select_asof(1, &["val"], Some(40)));
    let correct = naive_asof(&full, 40);
    let sabotaged = naive_asof(&sabotaged_history, 40);
    assert_eq!(engine, correct);
    assert!(engine.is_empty());
    assert_ne!(
        engine, sabotaged,
        "a reference missing the retraction must disagree with the engine"
    );
}

// ─────────────────────────────────────────────────────────────────────────
// Naive references for the recursive/join tasks (obviously correct).
// ─────────────────────────────────────────────────────────────────────────

/// The set of edges (as `[from, to]`) present as-of `at`.
fn naive_present_edges(edges: &[Version], at: i64) -> BTreeSet<(i64, i64)> {
    naive_asof(edges, at)
        .into_iter()
        .map(|row| (row[0].get_int().unwrap(), row[1].get_int().unwrap()))
        .collect()
}

/// Transitive closure of an edge set, as `[from, to]` tuples.
fn naive_transitive_closure(edges: &BTreeSet<(i64, i64)>) -> BTreeSet<Tuple> {
    let mut reach: BTreeSet<(i64, i64)> = edges.clone();
    loop {
        let mut added = false;
        let snapshot: Vec<(i64, i64)> = reach.iter().copied().collect();
        for &(a, b) in &snapshot {
            for &(c, d) in &snapshot {
                if b == c && reach.insert((a, d)) {
                    added = true;
                }
            }
        }
        if !added {
            break;
        }
    }
    reach.into_iter().map(|(a, b)| vec![v(a), v(b)]).collect()
}
