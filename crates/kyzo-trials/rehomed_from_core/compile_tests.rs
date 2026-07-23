/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Compile-tier differentials (RA plans vs oracle). Re-homed from
//! `kyzo-core::exec::plan::compile` tests (crate wall).

#![cfg(test)]

#[cfg(test)]
fn must<T, E: core::fmt::Debug>(r: Result<T, E>, door: &str) -> T {
    match r {
        Ok(v) => v,
        Err(e) => {
            assert!(false, "{door}: {e:?}");
            loop {}
        }
    }
}

#[cfg(test)]
fn must_some<T>(o: Option<T>, door: &str) -> T {
    match o {
        Some(v) => v,
        None => {
            assert!(false, "{door}");
            loop {}
        }
    }
}


use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::num::NonZeroU32;

use smartstring::SmartString;

use std::ops::ControlFlow;

use kyzo::oracle_harness::IndexPositionUse::{BindForLater, Ignored, Join};
use kyzo::oracle_harness::{
    AccessLevel, AtomOccurrence, BindingPos, Budget, CompiledProgram, CompiledRuleBody,
    CompiledRuleSet, EpochStore, IndexKind, IndexPositionUse, IndexRef, InsufficientAccessLevel,
    KeyspaceKind, MagicAtom, MagicInlineRule, MagicProgram, MagicRelationApplyAtom,
    MagicRuleApplyAtom, MagicRulesOrFixed, MagicSymbol, NoFixedRules, RelAlgebra, RelationHandle,
    RelationId, RowLimit, RuleBody, RulesetHeadAggrMismatch, Segments, StoreLifetimes,
    StoredRowTooShortError, StratifiedMagicProgram, TupleInIter, bind_for_eval, create_relation,
    set_access_level, stratified_evaluate, stratified_magic_compile,
};
use kyzo::{FjallStorage, Storage, WriteTx, new_fjall_storage};
use kyzo_model::SourceSpan;
use kyzo_model::program::aggregate::parse_aggr;
use kyzo_model::program::expr::Expr;
use kyzo_model::program::op::{OP_ADD, OP_LIST};
use kyzo_model::program::query::InputRelationHandle;
use kyzo_model::program::rule::{HeadAggrSlot, Unification};
use kyzo_model::program::symbol::Symbol;
use kyzo_model::schema::{ColType, ColumnDef, NullableColType, StoredRelationMetadata};
use kyzo_model::value::convert::{
    i64_bits_from_u64, i64_from_u64_fitting, i64_from_usize, usize_from_u64_fitting,
};
use kyzo_model::value::{DataValue, Tuple};
use kyzo_oracle::eval::{Literal, Program, Rel, Rule, Term, naive_eval};

// ── plumbing ─────────────────────────────────────────────────────────

#[cfg(test)]
fn to_engine_aggr(slot: &kyzo_oracle::HeadAggr) -> HeadAggrSlot {
    match slot {
        kyzo_oracle::HeadAggr::Plain => HeadAggrSlot::Plain,
        kyzo_oracle::HeadAggr::Aggregated { fold, args } => HeadAggrSlot::Aggregated {
            aggr: must_some(
                parse_aggr(fold.name()).ok().flatten(),
                "engine fold missing",
            ),
            args: args.clone(),
        },
    }
}

#[cfg(test)]
fn sp() -> SourceSpan {
    SourceSpan(0, 0)
}
#[cfg(test)]
fn sym(name: &str) -> Symbol {
    Symbol::new(name, sp())
}
#[cfg(test)]
fn v(i: i64) -> DataValue {
    DataValue::from(i)
}
#[cfg(test)]
fn muggle(rel: impl AsRef<str>) -> MagicSymbol {
    MagicSymbol::Muggle {
        inner: sym(rel.as_ref()),
    }
}
#[cfg(test)]
fn entry_symbol() -> MagicSymbol {
    MagicSymbol::Muggle {
        inner: Symbol::prog_entry(sp()),
    }
}
#[cfg(test)]
fn generous_budget() -> Budget {
    // Arm the derived-tuple ceiling as well as the epoch ceiling: a
    // differential run against a MUTATED plan (e.g. an eliminate that
    // never fires) can diverge, and eval checks this dimension at the
    // epoch barrier (eval.rs, typed LimitExceeded{DerivedTuples}). Every
    // legitimate corpus here admits well under 100 tuples in total, so
    // 1_000 gives 10x headroom and never refuses a real run.
    //
    // The number is deliberately modest, not "astronomically large": a
    // divergence that disables column elimination *widens* the tuples
    // every epoch, so the process exhausts memory before the CUMULATIVE
    // admitted count reaches a large ceiling. Measured under the test
    // memory cap, a ceiling of 1_000 trips into a typed refusal while
    // 10_000+ still allocation-aborts. Keep this low.
    Budget::new(NonZeroU32::new(10_000).expect("nonzero")).with_derived_tuple_ceiling(1_000)
}

/// A bounded-but-larger budget for the batch-boundary equivalence tests,
/// which deliberately build stores that straddle `BATCH_ROWS`=1024 (a
/// chain's `path` store, a wide relation of a few thousand rows) and so
/// legitimately need more than `generous_budget`'s intentionally-low
/// 1_000. Sized just above those workloads' real derived-tuple spend
/// (tens of thousands) and far below any OOM regime — the equivalence
/// tests run CORRECT plans (plus one row-dropping mutation, which only
/// shrinks the tuple set), so the OOM-before-ceiling hazard that keeps
/// `generous_budget` low does not apply here.
#[cfg(test)]
fn boundary_budget() -> Budget {
    Budget::new(NonZeroU32::new(10_000).expect("nonzero")).with_derived_tuple_ceiling(200_000)
}

#[cfg(test)]
fn col(name: &str) -> ColumnDef {
    ColumnDef {
        name: SmartString::from(name),
        typing: NullableColType::required(ColType::Any),
        default_gen: None,
    }
}

/// Create an all-key-columns stored relation and fill it with rows.
#[cfg(test)]
fn stored_relation(db: &FjallStorage, name: &str, arity: usize, rows: &[Tuple]) {
    let keys: Vec<ColumnDef> = (0..arity).map(|i| col(&format!("c{i}"))).collect();
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
    let handle = create_relation(&mut tx, input, KeyspaceKind::Facts).expect("create relation");
    for row in rows {
        handle
            .put_fact(
                &mut tx,
                row.as_slice(),
                kyzo_model::value::ValidityTs::of_micros(0),
                sp(),
            )
            .expect("put row");
    }
    tx.commit().expect("commit");
}

// Body-atom builders.
#[cfg(test)]
fn rule_atom(name: impl AsRef<str>, args: &[Symbol]) -> MagicAtom {
    MagicAtom::Rule(MagicRuleApplyAtom {
        name: muggle(name),
        args: args.to_vec(),
        span: sp(),
    })
}
#[cfg(test)]
fn neg_rule_atom(name: impl AsRef<str>, args: &[Symbol]) -> MagicAtom {
    MagicAtom::NegatedRule(MagicRuleApplyAtom {
        name: muggle(name),
        args: args.to_vec(),
        span: sp(),
    })
}
#[cfg(test)]
fn rel_atom(name: &str, args: &[Symbol]) -> MagicAtom {
    MagicAtom::Relation(MagicRelationApplyAtom {
        name: sym(name),
        args: args.to_vec(),
        validity: None,
        span: sp(),
    })
}
#[cfg(test)]
fn neg_rel_atom(name: &str, args: &[Symbol]) -> MagicAtom {
    MagicAtom::NegatedRelation(MagicRelationApplyAtom {
        name: sym(name),
        args: args.to_vec(),
        validity: None,
        span: sp(),
    })
}
#[cfg(test)]
fn unif(binding: Symbol, val: DataValue) -> MagicAtom {
    MagicAtom::Unification(Unification {
        binding,
        expr: Expr::Const { val, span: sp() },
        one_many_unif: false,
        span: sp(),
    })
}

#[cfg(test)]
fn plain_rule(head: &[Symbol], body: Vec<MagicAtom>) -> MagicInlineRule {
    MagicInlineRule {
        head: head.to_vec(),
        aggr: (0..head.len()).map(|_| HeadAggrSlot::Plain).collect(),
        body,
    }
}

#[cfg(test)]
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

/// Lifetimes: every store lives to the end (fine for tests; the real
/// map comes from the stratifier).
#[cfg(test)]
fn immortal_lifetimes(compiled: &[CompiledProgram]) -> StoreLifetimes {
    let mut lifetimes = StoreLifetimes::default();
    // INVARIANT(LastIndex): empty program floors last stratum at 0; else len-1.
    let last = compiled.len().saturating_sub(1);
    for stratum in compiled {
        for name in stratum.keys() {
            lifetimes.note_use(name.clone(), last);
        }
    }
    lifetimes
}

/// Compile against a read snapshot and evaluate to the entry rows, on
/// the classic iterator path.
#[cfg(test)]
fn compile_and_run(db: &FjallStorage, prog: StratifiedMagicProgram) -> BTreeSet<Tuple> {
    compile_and_run_mode(db, prog)
}

/// [`compile_and_run`] over a chosen execution mode. The differential
/// harness runs BOTH modes and asserts each equals the oracle, which is
/// what proves the batched (vectorized) path equivalent.
#[cfg(test)]
fn compile_and_run_mode(db: &FjallStorage, prog: StratifiedMagicProgram) -> BTreeSet<Tuple> {
    compile_and_run_mode_budget(db, prog, generous_budget())
}

/// [`compile_and_run_mode`] over an explicit budget — the batch-boundary
/// tests pass [`boundary_budget`] because they exceed the deliberately
/// low `generous_budget`.
#[cfg(test)]
fn compile_and_run_mode_budget(
    db: &FjallStorage,
    prog: StratifiedMagicProgram,
    budget: Budget,
) -> BTreeSet<Tuple> {
    let rtx = db.read_tx().expect("read tx");
    let compiled = stratified_magic_compile(&rtx, prog).expect("compiles");
    let lifetimes = immortal_lifetimes(&compiled);
    let program = bind_for_eval::<_, NoFixedRules>(&compiled, &rtx, Segments::OFF, &mut |_| {
        panic!("test programs have no fixed rules")
    })
    .expect("binds");
    let outcome = stratified_evaluate(&program, &lifetimes, RowLimit::default(), &budget, None)
        .expect("evaluates");
    outcome
        .store
        .all_iter()
        .expect("test store iter")
        .map(TupleInIter::try_into_tuple)
        .collect::<Result<BTreeSet<_>, _>>()
        .expect("test store rows")
}

#[cfg(test)]
fn rows(data: &[&[i64]]) -> BTreeSet<Tuple> {
    data.iter()
        .map(|r| r.iter().copied().map(v).collect())
        .collect()
}

// ── the upstream in-file test, ported ────────────────────────────────

/// The original ra.rs's `test_mat_join`, driven through the compile
/// tier over a real stored relation: `a = 3` binds `a` first, so
/// `data[x, a]` joins on data's SECOND column — the materialized-join
/// path.
#[test]
fn mat_join_reproduces_upstream_example() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    stored_relation(
        &db,
        "data",
        2,
        &[
            Tuple::from_vec(vec![v(1), v(2)]),
            Tuple::from_vec(vec![v(1), v(3)]),
            Tuple::from_vec(vec![v(2), v(3)]),
        ],
    );
    let (x, a) = (sym("x"), sym("a"));
    let prog = program_of(vec![vec![(
        entry_symbol(),
        vec![plain_rule(
            std::slice::from_ref(&x),
            vec![unif(a.clone(), v(3)), rel_atom("data", &[x.clone(), a])],
        )],
    )]]);
    assert_eq!(compile_and_run(&db, prog), rows(&[&[1], &[2]]));
}

// ── the first real-storage recursive query ───────────────────────────

/// Transitive closure through REAL RA operators against a stored
/// relation on a real FjallStorage: semi-naive recursion with the
/// delta threaded through `TempStoreRA`, base facts scanned from disk.
#[test]
fn transitive_closure_end_to_end_over_fjall() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    stored_relation(
        &db,
        "edge",
        2,
        &[
            Tuple::from_vec(vec![v(1), v(2)]),
            Tuple::from_vec(vec![v(2), v(3)]),
            Tuple::from_vec(vec![v(3), v(4)]),
            Tuple::from_vec(vec![v(4), v(2)]),
        ],
    );
    let (x, y, z) = (sym("x"), sym("y"), sym("z"));
    let prog = program_of(vec![
        vec![(
            muggle("path"),
            vec![
                plain_rule(
                    &[x.clone(), y.clone()],
                    vec![rel_atom("edge", &[x.clone(), y.clone()])],
                ),
                plain_rule(
                    &[x.clone(), y.clone()],
                    vec![
                        rel_atom("edge", &[x.clone(), z.clone()]),
                        rule_atom("path", &[z.clone(), y.clone()]),
                    ],
                ),
            ],
        )],
        vec![(
            entry_symbol(),
            vec![plain_rule(
                &[x.clone(), y.clone()],
                vec![rule_atom("path", &[x, y])],
            )],
        )],
    ]);
    // Reachability of 1→2→3→4→2 (cycle 2-3-4): from 1 everything but
    // 1; within the cycle every pair.
    assert_eq!(
        compile_and_run(&db, prog),
        rows(&[
            &[1, 2],
            &[1, 3],
            &[1, 4],
            &[2, 2],
            &[2, 3],
            &[2, 4],
            &[3, 2],
            &[3, 3],
            &[3, 4],
            &[4, 2],
            &[4, 3],
            &[4, 4],
        ])
    );
}

/// The head aligner emits a Reorder when the head order differs from
/// the body's binding order.
#[test]
fn head_reorder_alignment() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    stored_relation(&db, "edge", 2, &[Tuple::from_vec(vec![v(1), v(2)])]);
    let (x, y) = (sym("x"), sym("y"));
    let prog = program_of(vec![vec![(
        entry_symbol(),
        vec![plain_rule(
            &[y.clone(), x.clone()],
            vec![rel_atom("edge", &[x, y])],
        )],
    )]]);
    assert_eq!(compile_and_run(&db, prog), rows(&[&[2, 1]]));
}

// ── join-strategy paths ──────────────────────────────────────────────

#[cfg(test)]
fn join_types_of(ra: &RelAlgebra, out: &mut Vec<&'static str>) {
    match ra {
        RelAlgebra::Join(j) => {
            join_types_of(&j.left, out);
            join_types_of(&j.right, out);
            out.push(j.join_type().expect("join type"));
        }
        RelAlgebra::NegJoin(j) => {
            join_types_of(&j.left, out);
            out.push(j.join_type().expect("neg join type"));
        }
        RelAlgebra::Reorder(r) => join_types_of(&r.relation, out),
        RelAlgebra::Filter(f) => join_types_of(&f.parent, out),
        RelAlgebra::Search(s) => join_types_of(&s.parent, out),
        RelAlgebra::Unification(u) => join_types_of(&u.parent, out),
        RelAlgebra::Fixed(_)
        | RelAlgebra::TempStore(_)
        | RelAlgebra::Stored(_)
        | RelAlgebra::StoredWithValidity(_)
        | RelAlgebra::Spans(_)
        | RelAlgebra::Delta(_) => {}
    }
}

#[cfg(test)]
fn compiled_entry_join_types(db: &FjallStorage, prog: StratifiedMagicProgram) -> Vec<&'static str> {
    let rtx = db.read_tx().unwrap();
    let compiled = stratified_magic_compile(&rtx, prog).expect("compiles");
    let entry = compiled
        .last()
        .and_then(|s| s.get(&entry_symbol()))
        .expect("entry compiled");
    let CompiledRuleSet::Rules(rules) = entry else {
        panic!("entry is an inline rule");
    };
    let mut types = vec![];
    join_types_of(&rules.rules[0].relation, &mut types);
    types
}

/// The second body atom joins the stored relation on its FIRST key
/// column → prefix join; on its SECOND → materialized join. Both give
/// exactly the expected rows.
#[test]
fn join_strategies_prefix_vs_materialized() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    stored_relation(
        &db,
        "edge",
        2,
        &[
            Tuple::from_vec(vec![v(1), v(2)]),
            Tuple::from_vec(vec![v(2), v(3)]),
            Tuple::from_vec(vec![v(3), v(1)]),
        ],
    );
    let (x, y, z) = (sym("x"), sym("y"), sym("z"));

    // ?[x, z] := *edge[x, y], *edge[y, z] — second scan joined on col 0.
    let prefix_prog = program_of(vec![vec![(
        entry_symbol(),
        vec![plain_rule(
            &[x.clone(), z.clone()],
            vec![
                rel_atom("edge", &[x.clone(), y.clone()]),
                rel_atom("edge", &[y.clone(), z.clone()]),
            ],
        )],
    )]]);
    let types = compiled_entry_join_types(&db, prefix_prog);
    assert!(
        types.contains(&"stored_prefix_join"),
        "expected a stored prefix join, got {types:?}"
    );
    let prefix_prog = program_of(vec![vec![(
        entry_symbol(),
        vec![plain_rule(
            &[x.clone(), z.clone()],
            vec![
                rel_atom("edge", &[x.clone(), y.clone()]),
                rel_atom("edge", &[y.clone(), z.clone()]),
            ],
        )],
    )]]);
    assert_eq!(
        compile_and_run(&db, prefix_prog),
        rows(&[&[1, 3], &[2, 1], &[3, 2]])
    );

    // ?[x, z] := *edge[x, y], *edge[z, y] — second scan joined on col 1.
    let mat_prog = || {
        program_of(vec![vec![(
            entry_symbol(),
            vec![plain_rule(
                &[x.clone(), z.clone()],
                vec![
                    rel_atom("edge", &[x.clone(), y.clone()]),
                    rel_atom("edge", &[z.clone(), y.clone()]),
                ],
            )],
        )]])
    };
    let types = compiled_entry_join_types(&db, mat_prog());
    assert!(
        types.contains(&"stored_mat_join"),
        "expected a stored materialized join, got {types:?}"
    );
    assert_eq!(
        compile_and_run(&db, mat_prog()),
        rows(&[&[1, 1], &[2, 2], &[3, 3]])
    );
}

/// A join binding a stored relation's WHOLE key goes through the
/// point-lookup specialization of the prefix join.
#[test]
fn join_strategy_point_lookup() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    stored_relation(
        &db,
        "edge",
        2,
        &[
            Tuple::from_vec(vec![v(1), v(2)]),
            Tuple::from_vec(vec![v(2), v(3)]),
        ],
    );
    stored_relation(
        &db,
        "cand",
        2,
        &[
            Tuple::from_vec(vec![v(1), v(2)]),
            Tuple::from_vec(vec![v(1), v(3)]),
        ],
    );
    let (x, y) = (sym("x"), sym("y"));
    // ?[x, y] := *cand[x, y], *edge[x, y] — edge joined on both keys.
    let prog = program_of(vec![vec![(
        entry_symbol(),
        vec![plain_rule(
            &[x.clone(), y.clone()],
            vec![
                rel_atom("cand", &[x.clone(), y.clone()]),
                rel_atom("edge", &[x, y]),
            ],
        )],
    )]]);
    assert_eq!(compile_and_run(&db, prog), rows(&[&[1, 2]]));
}

/// Negation strategies: a negated stored relation joined on a key
/// prefix (stored_neg_prefix_join) vs on a non-prefix column
/// (stored_neg_mat_join); both semantics exact.
#[test]
fn neg_join_strategies() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    stored_relation(
        &db,
        "edge",
        2,
        &[
            Tuple::from_vec(vec![v(1), v(2)]),
            Tuple::from_vec(vec![v(2), v(3)]),
        ],
    );
    stored_relation(&db, "blocked", 2, &[Tuple::from_vec(vec![v(1), v(2)])]);
    stored_relation(&db, "sink", 2, &[Tuple::from_vec(vec![v(9), v(3)])]);
    let (x, y) = (sym("x"), sym("y"));

    // not *blocked[x, y]: negation joined on the full key prefix.
    let prefix_prog = || {
        program_of(vec![vec![(
            entry_symbol(),
            vec![plain_rule(
                &[x.clone(), y.clone()],
                vec![
                    rel_atom("edge", &[x.clone(), y.clone()]),
                    neg_rel_atom("blocked", &[x.clone(), y.clone()]),
                ],
            )],
        )]])
    };
    let types = compiled_entry_join_types(&db, prefix_prog());
    assert!(
        types.contains(&"stored_neg_prefix_join"),
        "expected stored_neg_prefix_join, got {types:?}"
    );
    assert_eq!(compile_and_run(&db, prefix_prog()), rows(&[&[2, 3]]));

    // not *sink[w, y] with w fresh: negation joined on column 1 only —
    // the materialized (set-probe) negation.
    let w = sym("w");
    let mat_prog = || {
        program_of(vec![vec![(
            entry_symbol(),
            vec![plain_rule(
                &[x.clone(), y.clone()],
                vec![
                    rel_atom("edge", &[x.clone(), y.clone()]),
                    neg_rel_atom("sink", &[w.clone(), y.clone()]),
                ],
            )],
        )]])
    };
    let types = compiled_entry_join_types(&db, mat_prog());
    assert!(
        types.contains(&"stored_neg_mat_join"),
        "expected stored_neg_mat_join, got {types:?}"
    );
    // (9, 3) blocks y = 3.
    assert_eq!(compile_and_run(&db, mat_prog()), rows(&[&[1, 2]]));
}

// ── typed refusals ───────────────────────────────────────────────────

#[test]
fn unknown_rule_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let x = sym("x");
    let prog = program_of(vec![vec![(
        entry_symbol(),
        vec![plain_rule(
            std::slice::from_ref(&x),
            vec![rule_atom("ghost", std::slice::from_ref(&x))],
        )],
    )]]);
    let rtx = db.read_tx().unwrap();
    let err = stratified_magic_compile(&rtx, prog).unwrap_err();
    assert!(err.to_string().contains("not found"), "{err:?}");
}

#[test]
fn rule_arity_mismatch_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    stored_relation(&db, "edge", 2, &[]);
    let x = sym("x");
    let prog = program_of(vec![vec![(
        entry_symbol(),
        vec![plain_rule(
            std::slice::from_ref(&x),
            vec![rel_atom("edge", std::slice::from_ref(&x))],
        )],
    )]]);
    let rtx = db.read_tx().unwrap();
    let err = stratified_magic_compile(&rtx, prog).unwrap_err();
    assert!(err.to_string().contains("Arity mismatch"), "{err:?}");
}

#[test]
fn unbound_head_symbol_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    stored_relation(&db, "edge", 2, &[]);
    let (x, y, q) = (sym("x"), sym("y"), sym("q"));
    // ?[x, q] := *edge[x, y] — q bound nowhere.
    let prog = program_of(vec![vec![(
        entry_symbol(),
        vec![plain_rule(&[x.clone(), q], vec![rel_atom("edge", &[x, y])])],
    )]]);
    let rtx = db.read_tx().unwrap();
    let err = stratified_magic_compile(&rtx, prog).unwrap_err();
    assert!(
        err.to_string().contains("in rule head is unbound"),
        "{err:?}"
    );
}

/// Trap (c) of the reconciliation notes: arg-level aggregation
/// signature equality is enforced at the compile tier too.
#[test]
fn head_aggr_mismatch_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    stored_relation(&db, "edge", 2, &[]);
    let (x, y) = (sym("x"), sym("y"));
    let min_aggr = parse_aggr("min").unwrap().expect("min exists");
    let with_aggr = MagicInlineRule {
        head: vec![x.clone(), y.clone()],
        aggr: vec![
            HeadAggrSlot::Plain,
            HeadAggrSlot::Aggregated {
                aggr: min_aggr,
                args: vec![],
            },
        ],
        body: vec![rel_atom("edge", &[x.clone(), y.clone()])],
    };
    let without_aggr = plain_rule(
        &[x.clone(), y.clone()],
        vec![rel_atom("edge", &[x.clone(), y.clone()])],
    );
    let entry_reader = plain_rule(&[x.clone(), y.clone()], vec![rule_atom("m", &[x, y])]);
    let prog = program_of(vec![
        vec![(muggle("m"), vec![with_aggr, without_aggr])],
        vec![(entry_symbol(), vec![entry_reader])],
    ]);
    let rtx = db.read_tx().unwrap();
    let err = stratified_magic_compile(&rtx, prog).unwrap_err();
    assert!(
        err.downcast_ref::<RulesetHeadAggrMismatch>().is_some(),
        "expected RulesetHeadAggrMismatch, got {err:?}"
    );
}

/// A below-ReadOnly (hidden) relation cannot be read by a query.
#[test]
fn hidden_relation_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    stored_relation(&db, "secret", 1, &[Tuple::from_vec(vec![v(1)])]);
    let mut tx = db.write_tx().unwrap();
    set_access_level(&mut tx, &sym("secret"), AccessLevel::Hidden).unwrap();
    tx.commit().unwrap();

    let x = sym("x");
    let prog = program_of(vec![vec![(
        entry_symbol(),
        vec![plain_rule(
            std::slice::from_ref(&x),
            vec![rel_atom("secret", std::slice::from_ref(&x))],
        )],
    )]]);
    let rtx = db.read_tx().unwrap();
    let err = stratified_magic_compile(&rtx, prog).unwrap_err();
    assert!(
        err.downcast_ref::<InsufficientAccessLevel>().is_some(),
        "expected InsufficientAccessLevel, got {err:?}"
    );
}

// ── the RA-vs-oracle differential ────────────────────────────────────
//
// This is the proof that seam implementation #2 (compiled RA plans)
// equals implementation #1 (the oracle-model harness in eval's tests):
// both are judged against the same sealed naive evaluator, on the same
// corpus shapes. The model compiler below mirrors eval's test harness,
// except that EDB relations become REAL stored relations on a real
// FjallStorage and rule bodies become compiled operator trees.

/// Stratum assignment for the model (duplicates the oracle's
/// Bellman-Ford edge rules; the oracle's own strata are sealed).
#[cfg(test)]
fn strata_of(program: &Program) -> HashMap<Rel, usize> {
    let mut classes: HashMap<Rel, (bool, bool)> = HashMap::new(); // (has_aggr, is_meet)
    {
        let mut per_head: HashMap<Rel, Vec<&Rule>> = HashMap::new();
        for rule in &program.rules {
            per_head
                .entry(rule.head_rel.clone())
                .or_default()
                .push(rule);
        }
        for (rel, rules) in per_head {
            let has_aggr = rules
                .iter()
                .any(|r| r.aggr.iter().any(|a| a.is_aggregated()));
            let is_meet = has_aggr
                && rules.iter().all(|r| {
                    r.aggr.iter().all(|a| match a.as_aggregated() {
                        None => true,
                        Some((aggregation, _)) => aggregation.is_meet(),
                    })
                });
            classes.insert(rel, (has_aggr, is_meet));
        }
    }
    let is_meet = |rel: &Rel| classes.get(rel).is_some_and(|c| c.1);
    let mut edges = Vec::new();
    for rule in &program.rules {
        let head = rule.head_rel.clone();
        let (has_aggr, head_meet) = classes[&head];
        for l in &rule.body {
            let forcing = if has_aggr {
                if head_meet && l.rel == *head {
                    l.is_negated()
                } else {
                    true
                }
            } else {
                l.is_negated() || is_meet(&l.rel)
            };
            edges.push((head.clone(), l.rel.clone(), forcing));
        }
    }
    let mut s: HashMap<Rel, usize> = HashMap::new();
    for rule in &program.rules {
        s.insert(rule.head_rel.clone(), 0);
        for l in &rule.body {
            s.insert(l.rel.clone(), 0);
        }
    }
    for rel in program.facts.keys() {
        s.insert(rel.clone(), 0);
    }
    let bound = s.len() + 1;
    for _ in 0..bound {
        let mut changed = false;
        for (head, dep, forcing) in &edges {
            let need = s[dep] + usize::from(*forcing);
            if s[head] < need {
                s.insert(head.clone(), need);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    s
}

const ENTRY_VARS: [&str; 8] = ["v0", "v1", "v2", "v3", "v4", "v5", "v6", "v7"];

/// Convert one model literal into a magic atom (plus prepended
/// unifications for constant arguments), mirroring what the normalize
/// tier does for real programs.
#[cfg(test)]
fn literal_atoms(
    l: &Literal,
    idb: &BTreeSet<Rel>,
    const_serial: &mut usize,
    out: &mut Vec<MagicAtom>,
) {
    let mut args = Vec::with_capacity(l.args.len());
    for t in &l.args {
        match t {
            Term::Var(name) => args.push(sym(name.as_ref())),
            Term::Const(c) => {
                let fresh = sym(&format!("*c{}", *const_serial));
                *const_serial += 1;
                out.push(unif(fresh.clone(), c.clone()));
                args.push(fresh);
            }
        }
    }
    let atom = match (idb.contains(&l.rel), l.is_negated()) {
        (true, false) => rule_atom(l.rel.as_ref(), &args),
        (true, true) => neg_rule_atom(l.rel.as_ref(), &args),
        (false, false) => rel_atom(l.rel.as_ref(), &args),
        (false, true) => neg_rel_atom(l.rel.as_ref(), &args),
    };
    out.push(atom);
}

/// Evaluate `target` of the model through the REAL pipeline tail:
/// stored EDB → compiled RA plans → semi-naive evaluation.
/// `#[cfg(test)]`: rehomed differential helper; ProductionOnly exemption
/// (file-level `#![cfg(test)]` is not item-scoped for the detector).
#[cfg(test)]
fn ra_eval(model: &Program, target: Rel, target_arity: usize) -> BTreeSet<Tuple> {
    assert!(
        model.fixed.is_empty(),
        "RA differential corpus has no fixed rules"
    );
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let idb: BTreeSet<Rel> = model.rules.iter().map(|r| r.head_rel.clone()).collect();
    for (rel, facts) in &model.facts {
        assert!(!idb.contains(rel), "facts under a rule head");
        let arity = match facts.iter().next() {
            Some(t) => t.len(),
            // Empty EDB extension still needs a declared arity; corpus uses 1.
            None => 1,
        };
        let rows: Vec<Tuple> = facts.iter().cloned().collect();
        stored_relation(&db, rel.as_ref(), arity, &rows);
    }

    let strata_map = strata_of(model);
    let entry_stratum = match strata_map.values().copied().max() {
        Some(m) => m + 1,
        // No strata ⇒ entry is stratum 0+1.
        None => 1,
    };
    let mut strata: Vec<Vec<(MagicSymbol, Vec<MagicInlineRule>)>> =
        (0..=entry_stratum).map(|_| Vec::new()).collect();

    let mut per_head: BTreeMap<Rel, Vec<&Rule>> = BTreeMap::new();
    for rule in &model.rules {
        per_head
            .entry(rule.head_rel.clone())
            .or_default()
            .push(rule);
    }
    let mut const_serial = 0usize;
    for (head, rules) in per_head {
        let stratum = strata_map[&head];
        let magic_rules: Vec<MagicInlineRule> = rules
            .iter()
            .map(|r| {
                let mut body = Vec::new();
                // Positives first, then negatives: negation is safe
                // only over bound variables (the reorder tier's job in
                // the real pipeline).
                for l in r.body.iter().filter(|l| !l.is_negated()) {
                    literal_atoms(l, &idb, &mut const_serial, &mut body);
                }
                for l in r.body.iter().filter(|l| l.is_negated()) {
                    literal_atoms(l, &idb, &mut const_serial, &mut body);
                }
                let head_syms: Vec<Symbol> = r
                    .head_args
                    .iter()
                    .map(|t| match t {
                        Term::Var(name) => sym(name.as_ref()),
                        Term::Const(_) => panic!("corpus heads are variables"),
                    })
                    .collect();
                MagicInlineRule {
                    head: head_syms,
                    aggr: r.aggr.iter().map(to_engine_aggr).collect(),
                    body,
                }
            })
            .collect();
        strata[stratum].push((muggle(head.as_ref()), magic_rules));
    }
    // The entry: ?[v0..vn] := target[v0..vn].
    let vars: Vec<Symbol> = ENTRY_VARS[..target_arity].iter().map(|s| sym(s)).collect();
    strata[entry_stratum].push((
        entry_symbol(),
        vec![plain_rule(&vars, vec![rule_atom(target.as_ref(), &vars)])],
    ));

    compile_and_run_mode(&db, program_of(strata))
}

/// THE differential: every IDB relation of the model, evaluated by the
/// real compile+eval pipeline over real storage, must equal the sealed
/// oracle's answer. A disagreement is a FINDING.
///
/// Both execution modes are checked: the classic iterator path AND the
/// batched (vectorized) path each equal the oracle. Because a shared
/// oracle pins both, this simultaneously proves the batched path
/// equal to the iterator path — the equivalence the vectorization ascent
/// rests on.
#[cfg(test)]
fn assert_ra_matches_oracle(model: &Program) {
    let oracle_db = naive_eval(model).expect("oracle accepts the program");
    let mut arities: BTreeMap<Rel, usize> = BTreeMap::new();
    for r in &model.rules {
        arities.insert(r.head_rel.clone(), r.head_args.len());
    }
    for rel in model
        .rules
        .iter()
        .map(|r| r.head_rel.clone())
        .collect::<BTreeSet<_>>()
    {
        let oracle_rows = match oracle_db.get(&rel) {
            Some(rows) => rows.clone(),
            // Missing IDB key is the empty extension.
            None => BTreeSet::new(),
        };
        let ra_rows = ra_eval(model, rel.clone(), arities[&rel]);
        assert_eq!(
            ra_rows, oracle_rows,
            "FINDING: RA-backed eval disagrees with the oracle on '{rel}'"
        );
    }
}

#[cfg(test)]
fn edge_facts(edges: &[(i64, i64)]) -> BTreeMap<Rel, BTreeSet<Tuple>> {
    let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = Default::default();
    facts.insert(
        "edge".into(),
        edges
            .iter()
            .map(|(a, b)| vec![v(*a), v(*b)])
            .map(Tuple::from_vec)
            .collect(),
    );
    facts
}

#[cfg(test)]
fn lit(rel: impl Into<Rel>, args: Vec<Term>, negated: bool) -> Literal {
    let rel = rel.into();
    if negated {
        Literal::neg(rel, args)
    } else {
        Literal::pos(rel, args)
    }
}
#[cfg(test)]
fn tx() -> Term {
    Term::var("X")
}
#[cfg(test)]
fn ty() -> Term {
    Term::var("Y")
}
#[cfg(test)]
fn tz() -> Term {
    Term::var("Z")
}

#[test]
fn differential_transitive_closure() {
    assert_ra_matches_oracle(&Program {
        rules: vec![
            Rule::plain(
                "path",
                vec![tx(), ty()],
                vec![lit("edge", vec![tx(), ty()], false)],
            ),
            Rule::plain(
                "path",
                vec![tx(), ty()],
                vec![
                    lit("edge", vec![tx(), tz()], false),
                    lit("path", vec![tz(), ty()], false),
                ],
            ),
        ],
        facts: edge_facts(&[(1, 2), (2, 3), (3, 4), (4, 2)]),
        ..Program::default()
    });
}

/// TC by self-join: `path` twice in one body → multiplicity Many →
/// the complete-re-run path of the delta discipline.
#[test]
fn differential_transitive_closure_self_join() {
    assert_ra_matches_oracle(&Program {
        rules: vec![
            Rule::plain(
                "path",
                vec![tx(), ty()],
                vec![lit("edge", vec![tx(), ty()], false)],
            ),
            Rule::plain(
                "path",
                vec![tx(), tz()],
                vec![
                    lit("path", vec![tx(), ty()], false),
                    lit("path", vec![ty(), tz()], false),
                ],
            ),
        ],
        facts: edge_facts(&[(1, 2), (2, 3), (3, 1), (3, 4)]),
        ..Program::default()
    });
}

/// THREE occurrences of the same store in one body (`path` appears
/// three times): the self-join scheme generalizes past two occurrences
/// because every occurrence with a changed dependency gets its own
/// independent delta pass — verified against the naive oracle through
/// the real compiled pipeline.
#[test]
fn differential_three_way_self_join() {
    assert_ra_matches_oracle(&Program {
        rules: vec![
            Rule::plain(
                "path",
                vec![tx(), ty()],
                vec![lit("edge", vec![tx(), ty()], false)],
            ),
            Rule::plain(
                "path",
                vec![tx(), Term::var("W")],
                vec![
                    lit("path", vec![tx(), ty()], false),
                    lit("path", vec![ty(), tz()], false),
                    lit("path", vec![tz(), Term::var("W")], false),
                ],
            ),
        ],
        facts: edge_facts(&[(1, 2), (2, 3), (3, 1), (3, 4), (4, 5)]),
        ..Program::default()
    });
}

/// Stratified negation: unreachable vertex pairs, negating a
/// recursive rule's store (mem_neg join paths) across a stratum
/// boundary.
#[test]
fn differential_stratified_negation() {
    assert_ra_matches_oracle(&Program {
        rules: vec![
            Rule::plain(
                "vert",
                vec![tx()],
                vec![lit("edge", vec![tx(), ty()], false)],
            ),
            Rule::plain(
                "vert",
                vec![ty()],
                vec![lit("edge", vec![tx(), ty()], false)],
            ),
            Rule::plain(
                "path",
                vec![tx(), ty()],
                vec![lit("edge", vec![tx(), ty()], false)],
            ),
            Rule::plain(
                "path",
                vec![tx(), ty()],
                vec![
                    lit("edge", vec![tx(), tz()], false),
                    lit("path", vec![tz(), ty()], false),
                ],
            ),
            Rule::plain(
                "unreach",
                vec![tx(), ty()],
                vec![
                    lit("vert", vec![tx()], false),
                    lit("vert", vec![ty()], false),
                    lit("path", vec![tx(), ty()], true),
                ],
            ),
        ],
        facts: edge_facts(&[(1, 2), (2, 3), (4, 4)]),
        ..Program::default()
    });
}

/// The self-join shape (a store mentioned TWICE in one body) through a
/// MEET-aggregation head, RA-BACKED (`compile_magic_rule_body` →
/// `TempStoreRA`/`incremental_meet_eval`) rather than eval.rs's
/// hand-rolled model harness (`differential_meet_self_join_many_
/// multiplicity`) — the review of issue #68's fix flagged that the
/// model-harness tests can't see bugs in the real compiled scan path
/// at all (confirmed: mutating `TempStoreRA::iter_batched`'s
/// `scan_epoch` test made this exact rule shape diverge from the
/// oracle while the model-harness suite stayed green). `m` appears
/// twice in the second rule's body — the case that used to collapse
/// to `ContainedRuleMultiplicity::Many` (a full non-delta re-run every
/// epoch) and now runs two independent per-occurrence delta passes.
#[test]
fn differential_meet_self_join_through_ra() {
    let named = |name: &str| kyzo_oracle::HeadAggr::named(name);
    let mut facts = edge_facts(&[(1, 2), (2, 3), (3, 1)]);
    facts.insert(
        "seed".into(),
        [(1, 5), (2, 7), (3, 9)]
            .iter()
            .map(|(k, l)| vec![v(*k), v(*l)])
            .map(Tuple::from_vec)
            .collect(),
    );
    assert_ra_matches_oracle(&Program {
        rules: vec![
            Rule::aggregated(
                "m",
                vec![tx(), ty()],
                vec![kyzo_oracle::HeadAggr::Plain, named("min")],
                vec![lit("seed", vec![tx(), ty()], false)],
            ),
            // m(x, min w) :- m(x, _), m(w', w), edge(w', x): node x
            // adopts any predecessor's value; `m` appears twice.
            Rule::aggregated(
                "m",
                vec![tx(), tz()],
                vec![kyzo_oracle::HeadAggr::Plain, named("min")],
                vec![
                    lit("m", vec![tx(), ty()], false),
                    lit("m", vec![Term::var("W"), tz()], false),
                    lit("edge", vec![Term::var("W"), tx()], false),
                ],
            ),
        ],
        facts,
        ..Program::default()
    });
}

/// Meet aggregation inside recursion: `min` folded epoch by epoch
/// through the MeetAggrStore, RA-backed.
#[test]
fn differential_meet_aggregation_in_recursion() {
    let named = |name: &str| kyzo_oracle::HeadAggr::named(name);
    let mut facts = edge_facts(&[(1, 2), (2, 3), (3, 1)]);
    facts.insert(
        "seed".into(),
        [vec![v(1), v(0)]]
            .into_iter()
            .map(Tuple::from_vec)
            .collect(),
    );
    assert_ra_matches_oracle(&Program {
        rules: vec![
            Rule::aggregated(
                "m",
                vec![tx(), ty()],
                vec![kyzo_oracle::HeadAggr::Plain, named("min")],
                vec![lit("seed", vec![tx(), ty()], false)],
            ),
            Rule::aggregated(
                "m",
                vec![ty(), tz()],
                vec![kyzo_oracle::HeadAggr::Plain, named("min")],
                vec![
                    lit("edge", vec![tx(), ty()], false),
                    lit("m", vec![tx(), tz()], false),
                ],
            ),
        ],
        facts,
        ..Program::default()
    });
}

/// Normal aggregation at a stratum boundary: `count` grouped by the
/// first column, folded once over the fixpoint beneath.
#[test]
fn differential_normal_aggregation() {
    let named = |name: &str| kyzo_oracle::HeadAggr::named(name);
    assert_ra_matches_oracle(&Program {
        rules: vec![
            Rule::plain(
                "path",
                vec![tx(), ty()],
                vec![lit("edge", vec![tx(), ty()], false)],
            ),
            Rule::plain(
                "path",
                vec![tx(), ty()],
                vec![
                    lit("edge", vec![tx(), tz()], false),
                    lit("path", vec![tz(), ty()], false),
                ],
            ),
            Rule::aggregated(
                "outdeg",
                vec![tx(), ty()],
                vec![kyzo_oracle::HeadAggr::Plain, named("count")],
                vec![lit("path", vec![tx(), ty()], false)],
            ),
        ],
        facts: edge_facts(&[(1, 2), (2, 3), (3, 1), (1, 3)]),
        ..Program::default()
    });
}

/// Constant arguments in body literals (desugared to unifications, as
/// the normalize tier does): filter and join paths together.
#[test]
fn differential_constant_arguments() {
    assert_ra_matches_oracle(&Program {
        rules: vec![
            Rule::plain(
                "from_one",
                vec![ty()],
                vec![lit("edge", vec![Term::Const(v(1)), ty()], false)],
            ),
            Rule::plain(
                "hop_from_one",
                vec![tz()],
                vec![
                    lit("from_one", vec![ty()], false),
                    lit("edge", vec![ty(), tz()], false),
                ],
            ),
        ],
        facts: edge_facts(&[(1, 2), (2, 3), (3, 4), (1, 4)]),
        ..Program::default()
    });
}

/// `contained_rules` is keyed by OCCURRENCE (position among
/// `Rule`/`NegatedRule` atoms), not by store name: a positive and a
/// negated occurrence of the same store get distinct occurrence ids,
/// and BOTH are entered into the map — this map is also
/// `StoreLifetimes`'s dependency source (`eval.rs`'s `note_use`), and a
/// store read only inside a negation is used just as much as one read
/// positively (dropping it would let its lifetime end before a later
/// stratum's negation reads it). Only the POSITIVE occurrence is ever
/// actually selected for delta narrowing in practice — negation always
/// reads totals, and stratification guarantees a negated dependency's
/// delta is empty by the time this body runs.
#[test]
fn contained_rules_keys_by_occurrence_not_name() {
    let (x, y) = (sym("x"), sym("y"));
    let rule = MagicInlineRule {
        head: vec![x.clone()],
        aggr: vec![HeadAggrSlot::Plain],
        body: vec![
            rule_atom("a", &[x.clone(), y.clone()]),     // occurrence 0
            neg_rule_atom("a", &[y.clone(), x.clone()]), // occurrence 1 (negated)
            rule_atom("b", &[x.clone(), y.clone()]),     // occurrence 2
            rel_atom("edge", &[x, y]),                   // not Rule/NegatedRule: no occurrence
        ],
    };
    let contained = rule.contained_rules();
    assert_eq!(
        contained,
        BTreeMap::from([
            (AtomOccurrence(0), muggle("a")),
            (AtomOccurrence(1), muggle("a")),
            (AtomOccurrence(2), muggle("b")),
        ]),
        "occurrence 1 (the negated `a`) is numbered AND entered — distinct \
         from occurrence 0's positive `a`, but both name store `a`"
    );
}

/// The self-join shape (`pt(...), pt(...)` — Andersen's `load`/`store`
/// rules, issue #68): the SAME store mentioned twice gets TWO
/// occurrences, each independently delta-selectable — the predecessor
/// name-keyed scheme collapsed these into one `Many` entry and lost
/// the ability to narrow either occurrence to a delta at all.
#[test]
fn contained_rules_gives_repeated_store_two_independent_occurrences() {
    let (x, y, z) = (sym("x"), sym("y"), sym("z"));
    let rule = MagicInlineRule {
        head: vec![x.clone(), z.clone()],
        aggr: vec![HeadAggrSlot::Plain, HeadAggrSlot::Plain],
        body: vec![
            rule_atom("pt", &[x.clone(), y.clone()]),
            rule_atom("pt", &[y, z]),
        ],
    };
    let contained = rule.contained_rules();
    assert_eq!(
        contained,
        BTreeMap::from([
            (AtomOccurrence(0), muggle("pt")),
            (AtomOccurrence(1), muggle("pt")),
        ]),
        "two occurrences of `pt`, keyed independently — not collapsed to one entry"
    );
}

/// NegJoin's join_type surfaces on compiled plans (in-memory rule
/// negation), completing the strategy-path coverage.
#[test]
fn neg_join_type_over_rule_store() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    stored_relation(&db, "edge", 2, &[Tuple::from_vec(vec![v(1), v(2)])]);
    let (x, y) = (sym("x"), sym("y"));
    let prog = program_of(vec![
        vec![(
            muggle("r"),
            vec![plain_rule(
                &[x.clone(), y.clone()],
                vec![rel_atom("edge", &[x.clone(), y.clone()])],
            )],
        )],
        vec![(
            entry_symbol(),
            vec![plain_rule(
                &[x.clone(), y.clone()],
                vec![
                    rel_atom("edge", &[x.clone(), y.clone()]),
                    neg_rule_atom("r", &[x.clone(), y.clone()]),
                ],
            )],
        )],
    ]);
    let types = compiled_entry_join_types(&db, prog);
    assert!(
        types.contains(&"mem_neg_prefix_join"),
        "expected mem_neg_prefix_join, got {types:?}"
    );
}

/// Negation against a RULE store joined on a NON-prefix column — the
/// set-probe anti-join (mem_neg_mat_join, TempStoreRA::neg_join's
/// materialized branch). `?[x] := s2[x, y], not s(w, y)` with `w` fresh
/// joins only on `y` (s's second column), so the probe set is s's
/// column-1 values and a left row survives iff its `y` is NOT among
/// them. The oracle's law-4 (fully-bound negated literals) cannot cover
/// this shape, so this direct query-result test pins the `contains`
/// sense of the probe: inverting it (`!contains`) yields the complement
/// rows and fails here.
#[test]
fn neg_join_rule_store_non_prefix_set_probe() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    // s2's rows (via an EDB feeder); s's rows (feeder) have column-1
    // value 20 only.
    stored_relation(
        &db,
        "es2",
        2,
        &[
            Tuple::from_vec(vec![v(1), v(10)]),
            Tuple::from_vec(vec![v(2), v(20)]),
            Tuple::from_vec(vec![v(3), v(20)]),
        ],
    );
    stored_relation(&db, "es", 2, &[Tuple::from_vec(vec![v(7), v(20)])]);
    let (x, y, w) = (sym("x"), sym("y"), sym("w"));
    let prog = || {
        program_of(vec![
            vec![
                (
                    muggle("s2"),
                    vec![plain_rule(
                        &[x.clone(), y.clone()],
                        vec![rel_atom("es2", &[x.clone(), y.clone()])],
                    )],
                ),
                (
                    muggle("s"),
                    vec![plain_rule(
                        &[w.clone(), y.clone()],
                        vec![rel_atom("es", &[w.clone(), y.clone()])],
                    )],
                ),
            ],
            vec![(
                entry_symbol(),
                vec![plain_rule(
                    std::slice::from_ref(&x),
                    vec![
                        rule_atom("s2", &[x.clone(), y.clone()]),
                        neg_rule_atom("s", &[w.clone(), y.clone()]),
                    ],
                )],
            )],
        ])
    };
    let types = compiled_entry_join_types(&db, prog());
    assert!(
        types.contains(&"mem_neg_mat_join"),
        "expected mem_neg_mat_join, got {types:?}"
    );
    // s's column-1 values = {20}; keep s2 rows whose y ∉ {20}: only the
    // (1, 10) row → x = 1. (An inverted probe would instead keep 2, 3.)
    assert_eq!(compile_and_run(&db, prog()), rows(&[&[1]]));
}

/// A materialized join whose RIGHT side is the recursive store itself:
/// `r(x, y) :- edge(x, z), r(y, z)` joins `r` on its column 1 (a
/// non-prefix column) → mem_mat_join with `r` as the right operand, so
/// the delta of `r` is read through `TempStoreRA::iter`'s full-scan
/// delta path (delta_all_iter). Emptying that path drops every
/// recursively-derived-through-the-right fact, so this differential vs
/// the sealed oracle fails under that mutation.
#[test]
fn differential_recursive_right_self_join() {
    let mut facts = edge_facts(&[(1, 2), (2, 3)]);
    facts.insert(
        "base".into(),
        [vec![v(5), v(2)]]
            .into_iter()
            .map(Tuple::from_vec)
            .collect(),
    );
    assert_ra_matches_oracle(&Program {
        rules: vec![
            Rule::plain(
                "r",
                vec![tx(), ty()],
                vec![lit("base", vec![tx(), ty()], false)],
            ),
            Rule::plain(
                "r",
                vec![tx(), ty()],
                vec![
                    lit("edge", vec![tx(), tz()], false),
                    lit("r", vec![ty(), tz()], false),
                ],
            ),
        ],
        facts,
        ..Program::default()
    });
}

// ── truncated stored rows are typed, never a slice panic (law 5) ──────
//
// `decode_tuple_from_kv`'s arity is a capacity hint only; a row decoded
// from a truncated stored value is SHORTER than the relation's arity.
// The join paths that index disk-decoded rows by position must surface
// that as a typed error, not an out-of-bounds abort.

/// Create a `num_keys`-key + `num_vals`-non-key relation and write ONE
/// deliberately truncated row: a valid key with an EMPTY stored value,
/// so it decodes to only its key columns (the non-key columns missing) —
/// a row shorter than the declared arity. Hostile stored bytes, in the
/// spirit of the storage tier's corruption tests.
#[cfg(test)]
fn relation_with_truncated_row(
    db: &FjallStorage,
    name: &str,
    num_keys: usize,
    num_vals: usize,
    key_vals: &[DataValue],
) {
    let keys: Vec<ColumnDef> = (0..num_keys).map(|i| col(&format!("k{i}"))).collect();
    let non_keys: Vec<ColumnDef> = (0..num_vals).map(|i| col(&format!("nk{i}"))).collect();
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
    let handle = create_relation(&mut tx, input, KeyspaceKind::Facts).expect("create relation");
    // An Assert row with a keys-only tuple: its payload is the empty
    // sequence, so the logical row decodes to `num_keys` columns —
    // `num_vals` short of the arity.
    handle
        .put_fact(
            &mut tx,
            key_vals,
            kyzo_model::value::ValidityTs::of_micros(0),
            sp(),
        )
        .expect("put truncated row");
    tx.commit().expect("commit");
}

/// Point-lookup join over a truncated row: `?[k, w] := *probe[k, w],
/// *rel[k, w]` binds `rel`'s whole key AND its non-key column, so `rel`
/// is reached by point lookup and the join then indexes the (missing)
/// non-key column of the short row. Typed error, not a panic.
#[test]
fn point_lookup_join_short_row_is_typed_error() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    stored_relation(&db, "probe", 2, &[Tuple::from_vec(vec![v(1), v(5)])]);
    // rel: one key column `k`, one non-key column `nk`; the stored row
    // for key 1 has no value, so it decodes to length 1.
    relation_with_truncated_row(&db, "rel", 1, 1, &[v(1)]);
    let (k, w) = (sym("k"), sym("w"));
    let prog = program_of(vec![vec![(
        entry_symbol(),
        vec![plain_rule(
            &[k.clone(), w.clone()],
            vec![
                rel_atom("probe", &[k.clone(), w.clone()]),
                rel_atom("rel", &[k, w]),
            ],
        )],
    )]]);
    let rtx = db.read_tx().unwrap();
    let compiled = stratified_magic_compile(&rtx, prog).expect("compiles");
    let lifetimes = immortal_lifetimes(&compiled);
    let program = bind_for_eval::<_, NoFixedRules>(&compiled, &rtx, Segments::OFF, &mut |_| {
        panic!("no fixed rules")
    })
    .expect("binds");
    let err = stratified_evaluate(
        &program,
        &lifetimes,
        RowLimit::default(),
        &generous_budget(),
        None,
    )
    .unwrap_err();
    assert!(
        err.downcast_ref::<StoredRowTooShortError>().is_some(),
        "expected StoredRowTooShortError, got {err:?}"
    );
}

/// Stored negation on a key prefix over a truncated row: `?[k, w] :=
/// *src[k, w], not *blk[k, w]` joins the negated `blk` on `k` and `w`;
/// the prefix anti-join scans `blk` by key and indexes its (missing)
/// non-key column of the short row. Typed error, not a panic.
#[test]
fn stored_neg_prefix_join_short_row_is_typed_error() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    stored_relation(&db, "src", 2, &[Tuple::from_vec(vec![v(1), v(5)])]);
    relation_with_truncated_row(&db, "blk", 1, 1, &[v(1)]);
    let (k, w) = (sym("k"), sym("w"));
    let prog = program_of(vec![vec![(
        entry_symbol(),
        vec![plain_rule(
            &[k.clone(), w.clone()],
            vec![
                rel_atom("src", &[k.clone(), w.clone()]),
                neg_rel_atom("blk", &[k, w]),
            ],
        )],
    )]]);
    let rtx = db.read_tx().unwrap();
    let compiled = stratified_magic_compile(&rtx, prog).expect("compiles");
    let lifetimes = immortal_lifetimes(&compiled);
    let program = bind_for_eval::<_, NoFixedRules>(&compiled, &rtx, Segments::OFF, &mut |_| {
        panic!("no fixed rules")
    })
    .expect("binds");
    let err = stratified_evaluate(
        &program,
        &lifetimes,
        RowLimit::default(),
        &generous_budget(),
        None,
    )
    .unwrap_err();
    assert!(
        err.downcast_ref::<StoredRowTooShortError>().is_some(),
        "expected StoredRowTooShortError, got {err:?}"
    );
}

// ── batched (vectorized) execution: the one machine ────────────────
//
// The seven `differential_*` tests above already assert BOTH modes equal
// the oracle (see `assert_ra_matches_oracle`). These add what a
// vectorized engine specifically lies about: batch-boundary arithmetic.
// `BATCH_ROWS` (ra.rs) is 1024; a correct batched scan/filter must be
// byte-identical to the iterator path at exactly the boundary, one
// either side, an empty stream, a single row, and a whole rejected
// batch. An off-by-one in the chunk loop or the filter's survivor count
// shows up here and nowhere in a round-numbers corpus.

/// `c1 > k` as a body predicate atom.
#[cfg(test)]
fn pred_gt(col: Symbol, k: i64) -> MagicAtom {
    MagicAtom::Predicate(Expr::Apply {
        op: kyzo_model::program::op::OP_GT,
        args: Box::new([
            Expr::Binding {
                var: col,
                tuple_pos: BindingPos::Unresolved,
            },
            Expr::Const {
                val: v(k),
                span: sp(),
            },
        ]),
        span: sp(),
    })
}

/// `?[c0, c1] := *w[c0, c1], c1 > threshold` — the batched
/// scan→filter→project pipeline end to end.
/// Cross-mode differential for BATCHED UNIFICATION (the campaign
/// generates no unify atoms, so this is its coverage): single and
/// spread forms across batch boundaries, plus per-row error identity
/// for a poison row landing mid-stream.
#[test]
fn batched_unification_matches_iterator() {
    let unify_prog = |multi: bool| -> StratifiedMagicProgram {
        let (c0, c1, w) = (sym("c0"), sym("c1"), sym("w"));
        let expr = if multi {
            Expr::Apply {
                op: OP_LIST,
                args: Box::new([
                    Expr::Binding {
                        var: c0.clone(),
                        tuple_pos: BindingPos::Unresolved,
                    },
                    Expr::Binding {
                        var: c1.clone(),
                        tuple_pos: BindingPos::Unresolved,
                    },
                ]),
                span: sp(),
            }
        } else {
            Expr::Apply {
                op: OP_ADD,
                args: Box::new([
                    Expr::Binding {
                        var: c0.clone(),
                        tuple_pos: BindingPos::Unresolved,
                    },
                    Expr::Binding {
                        var: c1.clone(),
                        tuple_pos: BindingPos::Unresolved,
                    },
                ]),
                span: sp(),
            }
        };
        program_of(vec![vec![(
            entry_symbol(),
            vec![plain_rule(
                &[c0.clone(), c1.clone(), w.clone()],
                vec![
                    rel_atom("w", &[c0, c1]),
                    MagicAtom::Unification(Unification {
                        binding: w,
                        expr,
                        one_many_unif: multi,
                        span: sp(),
                    }),
                ],
            )],
        )]])
    };
    // 2049 rows: straddles the 1024 batch boundary twice.
    for multi in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let rows: Vec<Tuple> = (0..2049i64)
            .map(|i| vec![v(i), v(i * 3)])
            .map(Tuple::from_vec)
            .collect();
        stored_relation(&db, "w", 2, &rows);
        let rows_out = compile_and_run_mode_budget(&db, unify_prog(multi), boundary_budget());
        assert_eq!(rows_out.len(), if multi { 2049 * 2 - 1 } else { 2049 });
    }
    // Error identity: a poison row (string in an arithmetic unify)
    // past the first batch boundary errors IDENTICALLY in both modes.
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let mut rows: Vec<Tuple> = (0..1500i64)
        .map(|i| vec![v(i), v(i)])
        .map(Tuple::from_vec)
        .collect();
    rows[1300][1] = DataValue::from("poison");
    stored_relation(&db, "w", 2, &rows);
    let run = || -> String {
        let rtx = db.read_tx().expect("read tx");
        let compiled = stratified_magic_compile(&rtx, unify_prog_err()).expect("compiles");
        let lifetimes = immortal_lifetimes(&compiled);
        let program = bind_for_eval::<_, NoFixedRules>(&compiled, &rtx, Segments::OFF, &mut |_| {
            panic!("no fixed rules")
        })
        .expect("binds");
        stratified_evaluate(
            &program,
            &lifetimes,
            RowLimit::default(),
            &boundary_budget(),
            None,
        )
        .expect_err("poison row must error")
        .to_string()
    };
    fn unify_prog_err() -> StratifiedMagicProgram {
        use kyzo_model::program::op::OP_ADD;
        let (c0, c1, w) = (sym("c0"), sym("c1"), sym("w"));
        program_of(vec![vec![(
            entry_symbol(),
            vec![plain_rule(
                &[c0.clone(), w.clone()],
                vec![
                    rel_atom("w", &[c0.clone(), c1.clone()]),
                    MagicAtom::Unification(Unification {
                        binding: w,
                        expr: Expr::Apply {
                            op: OP_ADD,
                            args: Box::new([
                                Expr::Binding {
                                    var: c0,
                                    tuple_pos: BindingPos::Unresolved,
                                },
                                Expr::Binding {
                                    var: c1,
                                    tuple_pos: BindingPos::Unresolved,
                                },
                            ]),
                            span: sp(),
                        },
                        one_many_unif: false,
                        span: sp(),
                    }),
                ],
            )],
        )]])
    }
    // One machine: the pin is determinism — two runs of the same
    // program yield the byte-identical refusal.
    assert_eq!(run(), run(), "error identity across runs");
}

#[cfg(test)]
fn scan_filter_prog(threshold: i64) -> StratifiedMagicProgram {
    let (c0, c1) = (sym("c0"), sym("c1"));
    program_of(vec![vec![(
        entry_symbol(),
        vec![plain_rule(
            &[c0.clone(), c1.clone()],
            vec![rel_atom("w", &[c0, c1.clone()]), pred_gt(c1, threshold)],
        )],
    )]])
}

/// Build a fresh fjall `w[c0, c1]` of `n` rows `[i, i]`, run the
/// scan+filter program on BOTH modes, and assert they are byte-identical
/// to each other and to the analytic answer (`i > threshold`). `n`
/// straddles the batch boundary; the surviving count does too.
#[cfg(test)]
fn assert_scan_filter_equiv(n: usize, threshold: i64) {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let n_i = i64_from_usize(n).expect("scan-filter n fits i64");
    let rows: Vec<Tuple> = (0..n_i)
        .map(|i| vec![v(i), v(i)])
        .map(Tuple::from_vec)
        .collect();
    stored_relation(&db, "w", 2, &rows);

    let batch_rows =
        compile_and_run_mode_budget(&db, scan_filter_prog(threshold), boundary_budget());
    let expected: BTreeSet<Tuple> = (0..n_i)
        .filter(|&i| i > threshold)
        .map(|i| vec![v(i), v(i)])
        .map(Tuple::from_vec)
        .collect();

    assert_eq!(
        batch_rows, expected,
        "batched scan+filter wrong result at n={n}, threshold={threshold}"
    );
}

#[test]
fn batched_scan_filter_boundary_sizes() {
    // BATCH_ROWS is 1024. Straddle the scan chunk boundary with n, and
    // the *survivor* boundary with the threshold. threshold = -1 keeps
    // all n rows (survivors straddle 1024 with the same n); threshold
    // near n/2 makes the filter reject roughly half.
    for &n in &[0usize, 1, 2, 1023, 1024, 1025, 2047, 2048, 2049, 4096, 4097] {
        // keep-all: survivors = n, exercises the scan chunk boundary
        assert_scan_filter_equiv(n, -1);
        // reject-most: a single survivor from a full batch, then a whole
        // rejected leading batch when n > 1024
        if n > 0 {
            let n_i = i64_from_usize(n).expect("n fits i64");
            assert_scan_filter_equiv(n, n_i - 2);
        }
        // reject-all: empty output through the whole pipeline
        assert_scan_filter_equiv(n, i64_from_usize(n).expect("n fits i64"));
    }
}

#[test]
fn batched_recursion_boundary_sizes() {
    // A chain of n edges builds a `path` rule store of n*(n+1)/2 rows.
    // What must cross BATCH_ROWS=1024 is the STORE the batched scan
    // reads — so sizes are chosen for the store to straddle 1024 (n=44 →
    // 990 rows, n=45 → 1035, n=46 → 1081, n=64 → 2080, n=90 → 4095), not
    // for n itself. That crosses the boundary inside semi-naive
    // recursion (the entry rule projects the >1024-row `path` total)
    // while the derived-tuple spend stays far under the test budget.
    // Iterator ≡ batched at each size.
    for &n in &[1usize, 44, 45, 46, 64, 90] {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let n_i = i64_from_usize(n).expect("recursion n fits i64");
        let edges: Vec<Tuple> = (0..n_i)
            .map(|i| vec![v(i), v(i + 1)])
            .map(Tuple::from_vec)
            .collect();
        stored_relation(&db, "edge", 2, &edges);
        let (x, y, z) = (sym("x"), sym("y"), sym("z"));
        let prog = || {
            program_of(vec![
                vec![(
                    muggle("path"),
                    vec![
                        plain_rule(
                            &[x.clone(), y.clone()],
                            vec![rel_atom("edge", &[x.clone(), y.clone()])],
                        ),
                        plain_rule(
                            &[x.clone(), y.clone()],
                            vec![
                                rel_atom("edge", &[x.clone(), z.clone()]),
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
        let rows_out = compile_and_run_mode_budget(&db, prog(), boundary_budget());
        // a chain of n edges has n*(n+1)/2 reachable pairs
        assert_eq!(
            rows_out.len(),
            n * (n + 1) / 2,
            "chain TC pair count at n={n}"
        );
    }
}

/// A tiny deterministic LCG — a seeded random-graph campaign without a
/// proptest harness, so it runs in the always-on suite under caps.
#[cfg(test)]
fn lcg(state: &mut u64) -> u64 {
    // INVARIANT(lcg64): Knuth LCG step is defined wrapping on u64.
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    *state >> 16
}

#[test]
fn batched_random_program_campaign() {
    // 120 seeded random small graphs, each run through the transitive
    // closure program on BOTH modes and the oracle. Iterator ≡ batched ≡
    // oracle for every one. This is the mini-campaign the vectorization
    // ascent's mutation test sabotages.
    for seed in 0u64..120 {
        // INVARIANT(test_seed_mix): property-test seed diffusion uses modular golden mix.
        let mut st = seed.wrapping_mul(0x9E3779B97F4A7C15).wrapping_add(1);
        let n_verts = 3 + i64_from_u64_fitting(lcg(&mut st) % 8).expect("verts"); // 3..10
        let n_edges = 2 + usize_from_u64_fitting(lcg(&mut st) % 14); // 2..15 edges
        let mut edge_set: BTreeSet<(i64, i64)> = BTreeSet::new();
        for _ in 0..n_edges {
            let a = i64_bits_from_u64(lcg(&mut st)) % n_verts;
            let b = i64_bits_from_u64(lcg(&mut st)) % n_verts;
            edge_set.insert((a, b));
        }
        let model = Program {
            rules: vec![
                Rule::plain(
                    "path",
                    vec![tx(), ty()],
                    vec![lit("edge", vec![tx(), ty()], false)],
                ),
                Rule::plain(
                    "path",
                    vec![tx(), ty()],
                    vec![
                        lit("edge", vec![tx(), tz()], false),
                        lit("path", vec![tz(), ty()], false),
                    ],
                ),
            ],
            facts: {
                let mut f: BTreeMap<Rel, BTreeSet<Tuple>> = Default::default();
                f.insert(
                    "edge".into(),
                    edge_set
                        .iter()
                        .map(|(a, b)| vec![v(*a), v(*b)])
                        .map(Tuple::from_vec)
                        .collect(),
                );
                f
            },
            ..Program::default()
        };
        // assert_ra_matches_oracle runs BOTH modes vs the oracle.
        assert_ra_matches_oracle(&model);
    }
}

/// The seam contract is stronger than set equality: `for_each_derivation`
/// Drives the entry rule's `CompiledRuleBody` directly and checks the
/// survivor COUNT against the analytic answer at batch boundaries.
/// (The row-vs-batch order comparison died with the iterator machine;
/// ordering itself is pinned by the byte-identity trials.)
#[test]
fn batched_stream_survivor_count_is_analytic() {
    for &(n, threshold) in &[
        (1023usize, -1i64),
        (1024, -1),
        (1025, -1),
        (2049, 1024),
        (2049, -1),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let n_i = i64_from_usize(n).expect("survivor n fits i64");
        let rows: Vec<Tuple> = (0..n_i)
            .map(|i| vec![v(i), v(i)])
            .map(Tuple::from_vec)
            .collect();
        stored_relation(&db, "w", 2, &rows);

        let rtx = db.read_tx().expect("read tx");
        let compiled =
            stratified_magic_compile(&rtx, scan_filter_prog(threshold)).expect("compiles");
        let entry = compiled
            .iter()
            .flat_map(|stratum| stratum.values())
            .find_map(|rs| match rs {
                CompiledRuleSet::Rules(rules) => Some(&rules.rules[0]),
                CompiledRuleSet::Fixed(_) => None,
            })
            .expect("an inline rule");

        let stores: BTreeMap<MagicSymbol, EpochStore> = BTreeMap::new();
        let body = CompiledRuleBody {
            plan: entry,
            tx: &rtx,
            segments: Segments::OFF,
        };
        let mut seen: Vec<Tuple> = Vec::new();
        body.for_each_derivation(&stores, None, false, &mut |t, _| {
            seen.push(Tuple::from_vec(t.into_owned()));
            Ok(ControlFlow::Continue(()))
        })
        .expect("derives");
        let survivors = (threshold.max(-1) + 1..n_i).count();
        assert_eq!(seen.len(), survivors, "survivor count at n={n}");
    }
}

/// Index selection over by-reference plain indices: longest bound
/// prefix wins, coverage decides the back-join, and the law-5 edges
/// (empty argument list, stale mapper) degrade to "no index".
/// Relocated from session/catalog (née runtime/relation) with
/// [`IndexPositionUse`] (story #350 T2).
#[test]
fn choose_index_prefers_longest_prefix_and_survives_edges() {
    fn col(name: &str, coltype: ColType) -> ColumnDef {
        ColumnDef {
            name: SmartString::from(name),
            typing: NullableColType::required(coltype),
            default_gen: None,
        }
    }
    fn input_handle(
        name: &str,
        keys: Vec<ColumnDef>,
        non_keys: Vec<ColumnDef>,
    ) -> InputRelationHandle {
        InputRelationHandle::from_metadata(name, StoredRelationMetadata { keys, non_keys })
    }

    let mut handle = RelationHandle::new_from_input(
        input_handle(
            "base",
            vec![col("a", ColType::Int), col("b", ColType::Int)],
            vec![col("c", ColType::Int)],
        ),
        RelationId::new(7).expect("below cap"),
        KeyspaceKind::Facts,
    );
    handle.indices = vec![
        IndexRef {
            name: SmartString::from("by_b"),
            kind: IndexKind::Plain { mapper: vec![1, 0] },
        },
        IndexRef {
            name: SmartString::from("by_c_b"),
            kind: IndexKind::Plain { mapper: vec![2, 1] },
        },
    ];

    assert!(handle.choose_index(&[Join, Join, Ignored], false).is_none());
    let (chosen, back_join) = handle
        .choose_index(&[Ignored, Join, Join], false)
        .expect("an index applies");
    assert_eq!(chosen.name, "by_c_b");
    assert!(!back_join);
    let (chosen, back_join) = handle
        .choose_index(&[BindForLater, Join, BindForLater], false)
        .expect("an index applies");
    assert_eq!(chosen.name, "by_b");
    assert!(back_join, "position 2 is not covered by by_b");
    assert!(handle.choose_index(&[], false).is_none());
    assert!(handle.choose_index(&[Ignored], false).is_none());
    let (chosen, _) = handle
        .choose_index(&[Ignored, Join, Join], true)
        .expect("by_c_b ends on the validity (last key) column");
    assert_eq!(chosen.name, "by_c_b");
}
