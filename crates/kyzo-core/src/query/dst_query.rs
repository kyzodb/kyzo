/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Deterministic simulation testing (DST) **up the query path**.
//!
//! The storage-seam DST (`storage/sim.rs`, exercised by `storage/tests.rs`)
//! proves the KV contract is seed-reproducible under faults, crashes, and
//! contention. This module carries that proof one tier up: it runs *compiled
//! Datalog programs* — recursion, joins, aggregation, stratified negation —
//! over [`SimStorage`], while the seeded fault plan injects storage errors,
//! and pins the query-visible laws the README promises:
//!
//! 1. **Read-path faults never lie.** With storage errors injected mid-scan,
//!    a query either returns the exact answer a clean store gives (a
//!    differential, plus a hand-checked anchor) or returns a typed error —
//!    never a wrong answer, never a panic, never a hang (the epoch ceiling
//!    and the outer timeout bound liveness).
//! 2. **Crash consistency is query-visible.** After a simulated process crash
//!    or power cut, committed-durable facts are visible to a post-recovery
//!    query and the committed-but-not-durable window matches the kernel
//!    contract *at the answer level*.
//! 3. **Snapshot isolation holds at the answer level.** A query's answer
//!    corresponds to exactly one consistent snapshot even under a concurrent
//!    writer: an invariant that a torn read would visibly break (a+b=const
//!    across two rows) holds in every answer.
//! 4. **Time travel under faults never tears history.** An as-of query mid
//!    fault-plan answers correctly or errors typed.
//! 5. **Determinism, characterized honestly.** Same seed, run twice, must
//!    produce a byte-identical sequence of (answer | typed-error) — and this
//!    module documents exactly where that holds and where the query engine's
//!    internal `rayon` parallelism breaks it (see `determinism`).
//!
//! ## Why the helpers are rebuilt here
//!
//! The compile tier's own tests (`query/compile.rs::tests`) hold the
//! program-builder plumbing and the `FjallStorage` differential, but those
//! helpers are private to that module. Rather than widen an engine module's
//! surface for a test, this module rebuilds the thin builder layer over the
//! same `pub(crate)` pipeline entry points (`stratified_magic_compile`,
//! `bind_for_eval`, `stratified_evaluate`). The reference answers are
//! hand-checked constants — an oracle wholly independent of the pipeline, so
//! a systemic pipeline bug cannot corrupt both sides of the differential.

#![cfg(test)]

use crate::engines::segments::Segments;
use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;

use miette::{Result, miette};
use smartstring::SmartString;

use crate::data::aggr::parse_aggr;
use crate::data::program::{
    InputRelationHandle, MagicAtom, MagicInlineRule, MagicProgram, MagicRelationApplyAtom,
    MagicRuleApplyAtom, MagicRulesOrFixed, MagicSymbol, StoreLifetimes, StratifiedMagicProgram,
    ValidityClause,
};
use crate::data::relation::{ColType, ColumnDef, NullableColType, StoredRelationMetadata};
use crate::data::span::SourceSpan;
use crate::data::symb::Symbol;
use crate::data::value::Tuple;

use crate::data::value::{AsOf, DataValue, ValidityTs};
use crate::query::compile::{
    CompiledProgram, NoFixedRules, bind_for_eval, stratified_magic_compile,
};
use crate::query::eval::{Budget, RowLimit, stratified_evaluate};
use crate::runtime::relation::KeyspaceKind;
use crate::runtime::relation::create_relation;
use crate::storage::sim::{FaultConfig, SimRng, SimStorage, for_each_seed};
use crate::storage::{Storage, WriteTx};

// ═════════════════════════════════════════════════════════════════════════
// Program-builder plumbing (rebuilt over the pub(crate) pipeline).
// ═════════════════════════════════════════════════════════════════════════

fn sp() -> SourceSpan {
    SourceSpan(0, 0)
}
fn sym(name: &str) -> Symbol {
    Symbol::new(name, sp())
}
fn v(i: i64) -> DataValue {
    DataValue::from(i)
}
fn muggle(rel: &str) -> MagicSymbol {
    MagicSymbol::Muggle { inner: sym(rel) }
}
fn entry_symbol() -> MagicSymbol {
    MagicSymbol::Muggle {
        inner: Symbol::prog_entry(sp()),
    }
}

/// Generous, but armed: the epoch ceiling bounds recursion (no hang), and the
/// derived-tuple ceiling converts any runaway divergence into a typed
/// `LimitExceeded` instead of an allocation abort. Both are orders of
/// magnitude above these corpora, so a real run is never refused.
fn generous_budget() -> Budget {
    Budget::new(NonZeroU32::new(10_000).expect("nonzero")).with_derived_tuple_ceiling(1_000_000)
}

fn col(name: &str) -> ColumnDef {
    ColumnDef {
        name: SmartString::from(name),
        typing: NullableColType {
            coltype: ColType::Any,
            nullable: false,
        },
        default_gen: None,
    }
}

fn rule_atom(name: &str, args: &[Symbol]) -> MagicAtom {
    MagicAtom::Rule(MagicRuleApplyAtom {
        name: muggle(name),
        args: args.to_vec(),
        span: sp(),
    })
}
fn neg_rule_atom(name: &str, args: &[Symbol]) -> MagicAtom {
    MagicAtom::NegatedRule(MagicRuleApplyAtom {
        name: muggle(name),
        args: args.to_vec(),
        span: sp(),
    })
}
fn rel_atom(name: &str, args: &[Symbol]) -> MagicAtom {
    MagicAtom::Relation(MagicRelationApplyAtom {
        name: sym(name),
        args: args.to_vec(),
        validity: None,
        span: sp(),
    })
}
fn rel_atom_asof(name: &str, args: &[Symbol], as_of: AsOf) -> MagicAtom {
    MagicAtom::Relation(MagicRelationApplyAtom {
        name: sym(name),
        args: args.to_vec(),
        validity: Some(ValidityClause::At(as_of)),
        span: sp(),
    })
}
type HeadAggr = Option<(crate::data::aggr::Aggregation, Vec<DataValue>)>;

fn plain_rule(head: &[Symbol], body: Vec<MagicAtom>) -> MagicInlineRule {
    MagicInlineRule {
        head: head.to_vec(),
        aggr: vec![None; head.len()],
        body,
    }
}
fn aggr_rule(head: &[Symbol], aggr: Vec<HeadAggr>, body: Vec<MagicAtom>) -> MagicInlineRule {
    MagicInlineRule {
        head: head.to_vec(),
        aggr,
        body,
    }
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

fn rows(data: &[&[i64]]) -> BTreeSet<Tuple> {
    data.iter()
        .map(|r| r.iter().copied().map(v).collect())
        .collect()
}

/// Create an all-key-columns stored relation and fill it — the fallible twin
/// of the compile tier's `stored_relation`. Every storage touch is `?`, so an
/// injected fault during setup surfaces as `Err` rather than a panic; callers
/// that want fault-free setup wrap this in [`populate_retrying`].
fn stored_relation<S: Storage>(db: &S, name: &str, arity: usize, rows: &[Tuple]) -> Result<()> {
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
    let mut tx = db.write_tx()?;
    let handle = create_relation(&mut tx, input, KeyspaceKind::Facts)?;
    for row in rows {
        handle.put_fact(&mut tx, row.as_slice(), ValidityTs::from_raw(0), sp())?;
    }
    tx.commit()?;
    Ok(())
}

/// Compile a program against a read snapshot and evaluate it to the entry
/// rows — the **fallible** twin of the compile tier's `compile_and_run`.
/// Every pipeline stage is `?`, so an injected storage fault at open, during
/// compilation's catalog reads, or during evaluation's scans becomes a typed
/// `Err` here. This is the read-path law's whole point: the fault must arrive
/// as a value, not a panic, and not a silently-wrong answer.
fn try_run<S: Storage>(db: &S, prog: StratifiedMagicProgram) -> Result<BTreeSet<Tuple>> {
    let rtx = db.read_tx()?;
    let compiled = stratified_magic_compile(&rtx, prog)?;
    let lifetimes = immortal_lifetimes(&compiled);
    let program = bind_for_eval::<_, NoFixedRules>(&compiled, &rtx, Segments::OFF, &mut |_| {
        // These corpora contain no fixed rules, so this is never reached; a
        // returned Err (not a panic) keeps the "no panics" invariant honest.
        Err(miette!("dst-query corpus has no fixed rules"))
    })?;
    let outcome = stratified_evaluate(
        &program,
        &lifetimes,
        RowLimit::default(),
        &generous_budget(),
        None,
    )?;
    Ok(outcome.store.all_iter().map(|t| t.into_tuple()).collect())
}

// ═════════════════════════════════════════════════════════════════════════
// The fixtures: each is (populate, program, hand-checked answer). Every shape
// of the evaluator is represented — recursion, join, grouped aggregation,
// stratified negation, and a multi-head stratum for the parallelism probe.
// ═════════════════════════════════════════════════════════════════════════

#[derive(Clone, Copy)]
struct Fixture {
    name: &'static str,
    populate: fn(&SimStorage) -> Result<()>,
    program: fn() -> StratifiedMagicProgram,
    expected: fn() -> BTreeSet<Tuple>,
}

// ── recursion: transitive closure over a cyclic edge relation ────────────

fn tc_populate(db: &SimStorage) -> Result<()> {
    stored_relation(
        db,
        "edge",
        2,
        &[
            Tuple::from_vec(vec![v(1), v(2)]),
            Tuple::from_vec(vec![v(2), v(3)]),
            Tuple::from_vec(vec![v(3), v(4)]),
            Tuple::from_vec(vec![v(4), v(2)]),
        ],
    )
}
fn tc_program() -> StratifiedMagicProgram {
    let (x, y, z) = (sym("x"), sym("y"), sym("z"));
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
                vec![rule_atom("path", &[x, y])],
            )],
        )],
    ])
}
fn tc_expected() -> BTreeSet<Tuple> {
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
}

// ── join: two-hop paths (edge ⋈ edge) ────────────────────────────────────

fn join_populate(db: &SimStorage) -> Result<()> {
    stored_relation(
        db,
        "edge",
        2,
        &[
            Tuple::from_vec(vec![v(1), v(2)]),
            Tuple::from_vec(vec![v(2), v(3)]),
            Tuple::from_vec(vec![v(3), v(4)]),
            Tuple::from_vec(vec![v(2), v(5)]),
        ],
    )
}
fn join_program() -> StratifiedMagicProgram {
    let (x, y, z) = (sym("x"), sym("y"), sym("z"));
    program_of(vec![vec![(
        entry_symbol(),
        vec![plain_rule(
            &[x.clone(), z.clone()],
            vec![
                rel_atom("edge", &[x.clone(), y.clone()]),
                rel_atom("edge", &[y.clone(), z.clone()]),
            ],
        )],
    )]])
}
fn join_expected() -> BTreeSet<Tuple> {
    // 1->2->3, 1->2->5, 2->3->4
    rows(&[&[1, 3], &[1, 5], &[2, 4]])
}

// ── aggregation: grouped min over a cost relation ────────────────────────

fn aggr_populate(db: &SimStorage) -> Result<()> {
    stored_relation(
        db,
        "cost",
        2,
        &[
            Tuple::from_vec(vec![v(1), v(5)]),
            Tuple::from_vec(vec![v(1), v(3)]),
            Tuple::from_vec(vec![v(1), v(8)]),
            Tuple::from_vec(vec![v(2), v(7)]),
            Tuple::from_vec(vec![v(2), v(2)]),
        ],
    )
}
fn aggr_program() -> StratifiedMagicProgram {
    let (x, y, m) = (sym("x"), sym("y"), sym("m"));
    let min = parse_aggr("min").expect("min aggregation exists");
    program_of(vec![
        vec![(
            muggle("mincost"),
            vec![aggr_rule(
                &[x.clone(), y.clone()],
                vec![None, Some((min, vec![]))],
                vec![rel_atom("cost", &[x.clone(), y.clone()])],
            )],
        )],
        vec![(
            entry_symbol(),
            vec![plain_rule(
                &[x.clone(), m.clone()],
                vec![rule_atom("mincost", &[x, m])],
            )],
        )],
    ])
}
fn aggr_expected() -> BTreeSet<Tuple> {
    rows(&[&[1, 3], &[2, 2]])
}

// ── stratified negation: pairs of nodes with no path between them ────────

fn neg_populate(db: &SimStorage) -> Result<()> {
    stored_relation(db, "node", 1, &[Tuple::from_vec(vec![v(1)]), Tuple::from_vec(vec![v(2)]), Tuple::from_vec(vec![v(3)])])?;
    // 1->2 only: 3 is isolated, and nothing reaches 1.
    stored_relation(db, "edge", 2, &[Tuple::from_vec(vec![v(1), v(2)])])
}
fn neg_program() -> StratifiedMagicProgram {
    let (x, y, z) = (sym("x"), sym("y"), sym("z"));
    program_of(vec![
        vec![(
            muggle("reach"),
            vec![
                plain_rule(
                    &[x.clone(), y.clone()],
                    vec![rel_atom("edge", &[x.clone(), y.clone()])],
                ),
                plain_rule(
                    &[x.clone(), y.clone()],
                    vec![
                        rel_atom("edge", &[x.clone(), z.clone()]),
                        rule_atom("reach", &[z.clone(), y.clone()]),
                    ],
                ),
            ],
        )],
        vec![(
            entry_symbol(),
            vec![plain_rule(
                &[x.clone(), y.clone()],
                vec![
                    rel_atom("node", std::slice::from_ref(&x)),
                    rel_atom("node", std::slice::from_ref(&y)),
                    neg_rule_atom("reach", &[x.clone(), y.clone()]),
                ],
            )],
        )],
    ])
}
fn neg_expected() -> BTreeSet<Tuple> {
    // reach = {(1,2)}; the complement over node×node (3×3=9) minus (1,2).
    rows(&[
        &[1, 1],
        &[1, 3],
        &[2, 1],
        &[2, 2],
        &[2, 3],
        &[3, 1],
        &[3, 2],
        &[3, 3],
    ])
}

/// The fixtures whose head sits in a **single-head** stratum: the evaluator's
/// `par_iter` over a stratum's rule heads dispatches one element, so their
/// storage ops are not raced across `rayon` workers. These are the fixtures
/// for which the query-level fault plan is order-stable (see `determinism`).
const SINGLE_HEAD_FIXTURES: &[Fixture] = &[
    Fixture {
        name: "transitive_closure",
        populate: tc_populate,
        program: tc_program,
        expected: tc_expected,
    },
    Fixture {
        name: "two_hop_join",
        populate: join_populate,
        program: join_program,
        expected: join_expected,
    },
    Fixture {
        name: "grouped_min_aggr",
        populate: aggr_populate,
        program: aggr_program,
        expected: aggr_expected,
    },
    Fixture {
        name: "stratified_negation",
        populate: neg_populate,
        program: neg_program,
        expected: neg_expected,
    },
];

// ── a multi-head stratum, for the parallelism / determinism probe ────────

fn multihead_populate(db: &SimStorage) -> Result<()> {
    stored_relation(db, "ea", 2, &[Tuple::from_vec(vec![v(1), v(2)]), Tuple::from_vec(vec![v(2), v(3)])])?;
    stored_relation(db, "eb", 2, &[Tuple::from_vec(vec![v(10), v(20)]), Tuple::from_vec(vec![v(20), v(30)])])
}
fn multihead_program() -> StratifiedMagicProgram {
    // pa and pb are independent recursive closures in ONE stratum: eval's
    // par_iter dispatches both heads across rayon workers.
    let (x, y, z) = (sym("x"), sym("y"), sym("z"));
    let closure = |edge: &str, head: &str| -> (MagicSymbol, Vec<MagicInlineRule>) {
        (
            muggle(head),
            vec![
                plain_rule(
                    &[x.clone(), y.clone()],
                    vec![rel_atom(edge, &[x.clone(), y.clone()])],
                ),
                plain_rule(
                    &[x.clone(), y.clone()],
                    vec![
                        rel_atom(edge, &[x.clone(), z.clone()]),
                        rule_atom(head, &[z.clone(), y.clone()]),
                    ],
                ),
            ],
        )
    };
    program_of(vec![
        vec![closure("ea", "pa"), closure("eb", "pb")],
        vec![(
            entry_symbol(),
            vec![
                plain_rule(
                    &[x.clone(), y.clone()],
                    vec![rule_atom("pa", &[x.clone(), y.clone()])],
                ),
                plain_rule(&[x.clone(), y.clone()], vec![rule_atom("pb", &[x, y])]),
            ],
        )],
    ])
}
fn multihead_expected() -> BTreeSet<Tuple> {
    rows(&[&[1, 2], &[1, 3], &[2, 3], &[10, 20], &[10, 30], &[20, 30]])
}
const MULTIHEAD_FIXTURE: Fixture = Fixture {
    name: "multihead_parallel",
    populate: multihead_populate,
    program: multihead_program,
    expected: multihead_expected,
};

// ═════════════════════════════════════════════════════════════════════════
// Shared harness helpers.
// ═════════════════════════════════════════════════════════════════════════

/// Setup must not be what a read-fault campaign measures, so absorb setup's
/// transient faults with a bounded retry (legitimate: the contract says reads
/// fault transiently and callers rerun). Faults stay armed for the *query*
/// that follows, whose single raw attempt is the actual observation. The
/// retry count is a pure function of the seed, so determinism is preserved.
fn populate_retrying(db: &SimStorage, f: impl Fn(&SimStorage) -> Result<()>) -> Result<()> {
    let mut last = Ok(());
    for _ in 0..100_000 {
        match f(db) {
            Ok(()) => return Ok(()),
            Err(e) => last = Err(e),
        }
    }
    last
}

/// Seed count, checked-in small and env-scalable for nightly campaigns
/// (`KYZO_DST_QUERY_SEEDS=5000 cargo test -p kyzo --release dst_query`) —
/// the same knob shape as the parser fuzz corpus's `PROPTEST_CASES`.
fn seeds(default: u64) -> u64 {
    std::env::var("KYZO_DST_QUERY_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

/// A per-seed observation of one fault-injected query run: the law is that
/// this is either the correct answer or a typed error — never a wrong answer
/// (the `Answer` arm carries the rows so the caller asserts equality) and
/// never a panic (a panic escapes to `for_each_seed`, which stamps the seed).
#[derive(PartialEq, Eq, Clone, Debug)]
enum Observed {
    Answer(BTreeSet<Tuple>),
    TypedError,
}

fn observe_faulted(fx: &Fixture, seed: u64, faults: FaultConfig) -> Observed {
    let db = SimStorage::with_faults(seed, faults);
    // Setup is retried to completion; if even that cannot get through (should
    // not happen at these rates), treat it as the typed-error arm rather than
    // a panic.
    if populate_retrying(&db, fx.populate).is_err() {
        return Observed::TypedError;
    }
    match try_run(&db, (fx.program)()) {
        Ok(ans) => Observed::Answer(ans),
        Err(_) => Observed::TypedError,
    }
}

// ═════════════════════════════════════════════════════════════════════════
// Capability 1 — read-path fault campaign: correct answer or typed error,
// never a wrong answer, never a panic, never a hang.
// ═════════════════════════════════════════════════════════════════════════

/// The headline law. For every fixture and a sweep of seeds, inject storage
/// read faults during the query and assert: a completed run equals the
/// hand-checked answer (which also equals a clean-store run), or the run
/// returns a typed error. A wrong answer is CRITICAL and pins its seed.
#[test]
fn read_fault_campaign_correct_or_typed_never_wrong() {
    // 3% read faults. The one-machine (vectorized) executor accumulates up
    // to a full batch of rows before yielding, so a query touches more
    // storage reads per observable outcome than the retired row-at-a-time
    // path did; at the old 12% density no stratified_negation seed
    // completed and the Answer arm went vacuous. 3% keeps BOTH arms well
    // populated across every fixture (the anti-vacuity asserts below are
    // the instrument that keeps this calibration honest).
    let faults = FaultConfig {
        read_fail_ppm: 30_000,
        ..Default::default()
    };
    let n = seeds(64);
    for fx in SINGLE_HEAD_FIXTURES
        .iter()
        .chain(std::iter::once(&MULTIHEAD_FIXTURE))
    {
        let expected = (fx.expected)();
        // Clean-store reference: the identical program+data with no faults.
        // Pinned against the hand-checked constant so the differential is not
        // vacuously "compare a thing to itself".
        {
            let db = SimStorage::new(0xC1EA_0000);
            populate_retrying(&db, fx.populate).expect("clean setup");
            let clean = try_run(&db, (fx.program)()).expect("clean run");
            assert_eq!(
                clean, expected,
                "fixture '{}': clean-store answer disagrees with the hand-checked oracle",
                fx.name
            );
        }
        // Single pass: assert the law and tally arms together. `for_each_seed`
        // re-panics on the first failing seed (stamping it), so the Cell
        // tallies are read only if every seed obeyed the law.
        let ok = std::cell::Cell::new(0u64);
        let errs = std::cell::Cell::new(0u64);
        for_each_seed(0..n, |seed| match observe_faulted(fx, seed, faults) {
            Observed::Answer(ans) => {
                assert_eq!(
                    ans, expected,
                    "CRITICAL: fixture '{}' returned a WRONG ANSWER under read faults \
                     (seed {seed}); a fault produced a silently-incorrect result",
                    fx.name
                );
                ok.set(ok.get() + 1);
            }
            Observed::TypedError => errs.set(errs.get() + 1),
        });
        // Anti-vacuity: both arms must actually fire, or the law is untested.
        assert!(
            ok.get() > 0,
            "fixture '{}': no seed completed — faults too dense, law untested on the Answer arm",
            fx.name
        );
        assert!(
            errs.get() > 0,
            "fixture '{}': no seed errored — faults never reached the query, error arm untested",
            fx.name
        );
    }
}

// ═════════════════════════════════════════════════════════════════════════
// Capability 2 — crash consistency for query-visible state.
// ═════════════════════════════════════════════════════════════════════════

/// A committed-durable fact is visible to a query after a power cut; a
/// buffer-tier commit made after the last fsync is NOT — and the distinction
/// is asserted at the QUERY-ANSWER level, not just the KV level. A process
/// crash preserves every commit of both tiers. The kernel contract
/// (`storage/tests.rs::sim_campaign_durability_tiers_are_distinct`) pins this
/// at the KV seam; here the same contract is read through the query.
#[test]
fn crash_consistency_is_query_visible() {
    // Build "edge" durably, then add one buffer-tier edge and one
    // durable edge, tracking which survive each failure mode.
    let build = || {
        let db = SimStorage::new(7);
        // Durable base: 1->2, 2->3.
        let mut tx = db.write_tx().unwrap();
        let h = create_relation(
            &mut tx,
            InputRelationHandle {
                name: sym("edge"),
                metadata: StoredRelationMetadata {
                    keys: vec![col("c0"), col("c1")],
                    non_keys: vec![],
                },
                key_bindings: vec![sym("c0"), sym("c1")],
                dep_bindings: vec![],
                span: sp(),
            },
            KeyspaceKind::Facts,
        )
        .unwrap();
        let put = |tx: &mut crate::storage::sim::SimWriteTx, a: i64, b: i64| {
            let row = vec![v(a), v(b)];
            h.put_fact(tx, &row, ValidityTs::from_raw(0), sp()).unwrap();
        };
        put(&mut tx, 1, 2);
        put(&mut tx, 2, 3);
        tx.commit_durable().unwrap(); // durable: base + relation catalog

        // Buffer-tier edge 3->4 (survives crash, lost on power cut).
        let mut tx = db.write_tx().unwrap();
        put(&mut tx, 3, 4);
        tx.commit().unwrap();
        db
    };

    // The transitive closure a query would compute over each surviving store.
    let tc_over = |db: &SimStorage| -> BTreeSet<Tuple> {
        try_run(db, tc_program_named("edge")).expect("post-recovery query")
    };

    // Process crash: every commit of both tiers survives → 3->4 present, so
    // the closure reaches 4 from 1,2,3.
    let crashed = build().sim_crash();
    assert_eq!(
        tc_over(&crashed),
        rows(&[&[1, 2], &[1, 3], &[1, 4], &[2, 3], &[2, 4], &[3, 4],]),
        "after a process crash the buffer-tier edge 3->4 must be query-visible"
    );

    // Power cut: only the fsynced prefix survives → 3->4 is gone, so the
    // closure is exactly the durable base's reachability.
    let cut = build().sim_powercut();
    assert_eq!(
        tc_over(&cut),
        rows(&[&[1, 2], &[1, 3], &[2, 3]]),
        "after a power cut the un-fsynced edge 3->4 must NOT be query-visible, \
         and the answer must be exactly the durable prefix's closure"
    );
}

/// The transitive-closure program over an arbitrarily-named edge relation.
fn tc_program_named(edge: &str) -> StratifiedMagicProgram {
    let (x, y, z) = (sym("x"), sym("y"), sym("z"));
    program_of(vec![
        vec![(
            muggle("path"),
            vec![
                plain_rule(
                    &[x.clone(), y.clone()],
                    vec![rel_atom(edge, &[x.clone(), y.clone()])],
                ),
                plain_rule(
                    &[x.clone(), y.clone()],
                    vec![
                        rel_atom(edge, &[x.clone(), z.clone()]),
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
    ])
}

/// Crash consistency under a seeded fault plan: for each seed, build the store
/// with faults armed (setup retried), crash or power-cut per a seed-derived
/// choice, and assert the post-recovery query either answers with a value
/// drawn from the recovered prefix or errors typed — never a torn/impossible
/// answer, never a panic.
#[test]
fn crash_recovery_under_faults_never_tears() {
    let faults = FaultConfig {
        read_fail_ppm: 80_000,
        sync_fail_ppm: 80_000,
        ..Default::default()
    };
    let n = seeds(48);
    // The only two answers a correct recovery can yield: base-only closure
    // (power cut lost 3->4, or crash also lost it if the buffer commit never
    // landed) or the full closure (3->4 survived). Any OTHER answer is a torn
    // history read.
    let base = rows(&[&[1, 2], &[1, 3], &[2, 3]]);
    let full = rows(&[&[1, 2], &[1, 3], &[1, 4], &[2, 3], &[2, 4], &[3, 4]]);
    for_each_seed(0..n, |seed| {
        let db = SimStorage::with_faults(seed, faults);
        // Build the durable base, retrying setup through faults; keep the
        // handle so the buffer-tier edge can be added afterwards.
        let make_handle = || -> Result<crate::runtime::relation::RelationHandle> {
            let mut tx = db.write_tx()?;
            let h = create_relation(
                &mut tx,
                InputRelationHandle {
                    name: sym("edge"),
                    metadata: StoredRelationMetadata {
                        keys: vec![col("c0"), col("c1")],
                        non_keys: vec![],
                    },
                    key_bindings: vec![sym("c0"), sym("c1")],
                    dep_bindings: vec![],
                    span: sp(),
                },
                KeyspaceKind::Facts,
            )?;
            for (a, b) in [(1, 2), (2, 3)] {
                let row = vec![v(a), v(b)];
                h.put_fact(&mut tx, &row, ValidityTs::from_raw(0), sp())?;
            }
            tx.commit_durable()?;
            Ok(h)
        };
        // Retry the durable base until it lands (bounded).
        let mut handle = None;
        for _ in 0..100_000 {
            if let Ok(h) = make_handle() {
                handle = Some(h);
                break;
            }
        }
        let Some(h) = handle else {
            return; // could not establish the base; nothing to assert
        };
        // A buffer-tier edge 3->4 whose survival depends on the failure mode.
        // Its own landing is best-effort: if a fault aborts it, it simply did
        // not commit, which is one of the two legal recovered states.
        if let Ok(mut tx) = db.write_tx() {
            let row = vec![v(3), v(4)];
            if h.put_fact(&mut tx, &row, ValidityTs::from_raw(0), sp())
                .is_ok()
            {
                let _ = tx.commit(); // buffer tier; ignore fault
            }
        }
        // Recover both ways; each post-recovery query must be a clean prefix
        // answer or a typed error — never a torn history read.
        for recovered in [db.sim_crash(), db.sim_powercut()] {
            // A typed error is an allowed recovered state; a wrong answer is not.
            if let Ok(ans) = try_run(&recovered, tc_program_named("edge")) {
                assert!(
                    ans == base || ans == full,
                    "seed {seed}: torn history read — post-recovery answer {ans:?} \
                     is neither the base prefix nor the full closure"
                );
            }
        }
    });
}

// ═════════════════════════════════════════════════════════════════════════
// Capability 3 — snapshot isolation at the query-answer level.
// ═════════════════════════════════════════════════════════════════════════

/// A query's answer must correspond to exactly one consistent snapshot, even
/// while a writer commits concurrently. Construct the detector: two rows whose
/// values always sum to a constant (a=k, b=C-k); a writer flips k across many
/// commits, each preserving a+b=C. A reader runs a query that returns both
/// rows; a torn read (row a from one commit, row b from another) would yield
/// a+b≠C — a detectably impossible answer.
///
/// `SimReadTx` snapshots at open and compile+eval share that one snapshot, so
/// this SHOULD hold by construction. The test proves the construction with
/// genuine concurrency across many rounds. (Note: the query engine's internal
/// `rayon` workers are not scheduler participants, so the deterministic
/// token-barrier driver cannot interleave a query's *internal* reads; genuine
/// OS-thread concurrency is used here instead. See `determinism` and the
/// report for why.)
#[test]
fn snapshot_isolation_holds_at_answer_level() {
    const C: i64 = 1000;
    let db = SimStorage::new(42);
    // Relation "pair" with key = slot (0 or 1), value column carrying the
    // number. Seed a=0 -> (0,0),(1,1000).
    let h = {
        let mut tx = db.write_tx().unwrap();
        let h = create_relation(
            &mut tx,
            InputRelationHandle {
                name: sym("pair"),
                metadata: StoredRelationMetadata {
                    keys: vec![col("slot")],
                    non_keys: vec![col("num")],
                },
                key_bindings: vec![sym("slot")],
                dep_bindings: vec![sym("num")],
                span: sp(),
            },
            KeyspaceKind::Facts,
        )
        .unwrap();
        for (slot, num) in [(0i64, 0i64), (1, C)] {
            let row = vec![v(slot), v(num)];
            h.put_fact(&mut tx, &row, ValidityTs::from_raw(0), sp())
                .unwrap();
        }
        tx.commit().unwrap();
        h
    };

    // The query: ?[slot, num] := *pair[slot, num]. Answer is the two rows;
    // the test checks num(0)+num(1)==C.
    let read_pair = || -> BTreeSet<Tuple> {
        let (slot, num) = (sym("slot"), sym("num"));
        let prog = program_of(vec![vec![(
            entry_symbol(),
            vec![plain_rule(
                &[slot.clone(), num.clone()],
                vec![rel_atom("pair", &[slot, num])],
            )],
        )]]);
        try_run(&db, prog).expect("snapshot read")
    };

    let writer = {
        let db = db.clone();
        let h = h.clone();
        move || {
            let mut rng = SimRng::new(0xB0B0);
            for _ in 0..200 {
                let k = rng.below(C as u64 + 1) as i64;
                // Update both rows atomically in one commit: a=k, b=C-k.
                let mut done = false;
                for _ in 0..1000 {
                    let mut tx = match db.write_tx() {
                        Ok(t) => t,
                        Err(_) => continue,
                    };
                    let mut good = true;
                    for (slot, num) in [(0i64, k), (1, C - k)] {
                        let row = vec![v(slot), v(num)];
                        // Same valid instant every round: each atomic pair
                        // update is a system-time correction of the pair,
                        // and the current read resolves to the newest one.
                        if h.put_fact(&mut tx, &row, ValidityTs::from_raw(0), sp())
                            .is_err()
                        {
                            good = false;
                            break;
                        }
                    }
                    if good && tx.commit().is_ok() {
                        done = true;
                        break;
                    }
                }
                assert!(done, "writer could not land an atomic pair update");
            }
        }
    };

    std::thread::scope(|s| {
        s.spawn(writer);
        // Reader: many queries during the writer's run; each answer must sum
        // to C exactly. A torn read across two commits would break it.
        for _ in 0..200 {
            let ans = read_pair();
            let nums: BTreeMap<i64, i64> = ans
                .iter()
                .map(|t| {
                    let slot = t[0].get_int().expect("int slot");
                    let num = t[1].get_int().expect("int num");
                    (slot, num)
                })
                .collect();
            // The relation always has both slots; the sum is the invariant.
            let sum: i64 = nums.values().sum();
            assert_eq!(
                sum, C,
                "TORN READ: a query answer {nums:?} summed to {sum}, not {C} — \
                 the answer mixed two writers' snapshots"
            );
        }
    });
}

// ═════════════════════════════════════════════════════════════════════════
// Capability 4 — time travel under faults.
// ═════════════════════════════════════════════════════════════════════════

/// An as-of query mid fault-plan answers correctly or errors typed — never a
/// torn history read. The relation carries a validity stamp in its last key
/// slot; the query asks for the state as of a fixed time while read faults
/// fire. The correct answer is the newest assertive version at or before that
/// time; the differential is against a clean-store as-of run, anchored by a
/// hand-checked constant.
#[test]
fn time_travel_under_faults_answers_or_errors() {
    // Populate a validity relation: key = (id, validity), value = state.
    // id 1: asserted "a" @10, retracted @20, asserted "c" @30.
    // As of 25, id 1 is retracted (absent); as of 35, it is "c"; as of 15,
    // it is "a".
    let populate = |db: &SimStorage| -> Result<()> {
        let mut tx = db.write_tx()?;
        let h = create_relation(
            &mut tx,
            InputRelationHandle {
                name: sym("hist"),
                metadata: StoredRelationMetadata {
                    keys: vec![col("id")],
                    non_keys: vec![col("state")],
                },
                key_bindings: vec![sym("id")],
                dep_bindings: vec![sym("state")],
                span: sp(),
            },
            KeyspaceKind::Facts,
        )?;
        for (id, at, assertive, state) in [
            (1i64, 10i64, true, "a"),
            (1, 20, false, ""),
            (1, 30, true, "c"),
        ] {
            if assertive {
                let row = vec![v(id), DataValue::Str((*state).to_string())];
                h.put_fact(&mut tx, &row, ValidityTs::from_raw(at), sp())?;
            } else {
                h.retract_fact(&mut tx, &[v(id)], ValidityTs::from_raw(at), sp())?;
            }
        }
        tx.commit()
    };

    let asof_program = |at: i64| -> StratifiedMagicProgram {
        let (id, state) = (sym("id"), sym("state"));
        // The time slots are infrastructure: the atom binds user columns
        // only, and `as_of` supplies the coordinate.
        program_of(vec![vec![(
            entry_symbol(),
            vec![plain_rule(
                &[id.clone(), state.clone()],
                vec![rel_atom_asof(
                    "hist",
                    &[id, state],
                    AsOf::current(ValidityTs::from_raw(at)),
                )],
            )],
        )]])
    };

    // Clean-store anchors.
    {
        let db = SimStorage::new(0xA50F);
        populate_retrying(&db, |d| populate(d)).expect("clean setup");
        assert_eq!(
            try_run(&db, asof_program(15)).expect("asof 15"),
            rows_str(&[(1, "a")]),
            "as of 15, id 1 is asserted 'a'"
        );
        // as of 25 the latest version is a retraction → absent.
        assert_eq!(
            try_run(&db, asof_program(25)).expect("asof 25"),
            BTreeSet::new(),
            "as of 25, id 1 is retracted and must be absent"
        );
        assert_eq!(
            try_run(&db, asof_program(35)).expect("asof 35"),
            rows_str(&[(1, "c")]),
            "as of 35, id 1 is asserted 'c'"
        );
    }

    // Under faults: each as-of answer is the clean answer or a typed error.
    let faults = FaultConfig {
        read_fail_ppm: 150_000,
        ..Default::default()
    };
    let n = seeds(48);
    let ok = std::cell::Cell::new(0u64);
    let errs = std::cell::Cell::new(0u64);
    for at in [15i64, 25, 35] {
        let expected = {
            let db = SimStorage::new(0xA50F);
            populate_retrying(&db, |d| populate(d)).unwrap();
            try_run(&db, asof_program(at)).unwrap()
        };
        for_each_seed(0..n, |seed| {
            let db = SimStorage::with_faults(seed, faults);
            if populate_retrying(&db, |d| populate(d)).is_err() {
                errs.set(errs.get() + 1);
                return;
            }
            match try_run(&db, asof_program(at)) {
                Ok(ans) => {
                    assert_eq!(
                        ans, expected,
                        "CRITICAL: as-of {at} torn under faults (seed {seed})"
                    );
                    ok.set(ok.get() + 1);
                }
                Err(_) => errs.set(errs.get() + 1),
            }
        });
    }
    assert!(ok.get() > 0, "as-of Answer arm never fired");
    assert!(errs.get() > 0, "as-of error arm never fired");
}

fn rows_str(data: &[(i64, &str)]) -> BTreeSet<Tuple> {
    data.iter()
        .map(|(id, s)| vec![v(*id), DataValue::Str((*s).to_string())])
        .map(Tuple::from_vec).collect()
}

// ═════════════════════════════════════════════════════════════════════════
// Capability 5 — determinism of the simulation at the query level.
// ═════════════════════════════════════════════════════════════════════════

/// Same seed, run twice, byte-identical observable — for the single-head
/// fixtures, whose storage ops are not raced across rayon workers. This is
/// the property the storage seam guarantees, lifted to the query answer.
#[test]
fn determinism_holds_for_single_head_queries() {
    let faults = FaultConfig {
        read_fail_ppm: 120_000,
        ..Default::default()
    };
    let n = seeds(32);
    for fx in SINGLE_HEAD_FIXTURES {
        for seed in 0..n {
            let a = observe_faulted(fx, seed, faults);
            let b = observe_faulted(fx, seed, faults);
            assert_eq!(
                a, b,
                "fixture '{}' seed {seed}: same seed produced different observables — \
                 the query-level simulation is not reproducible",
                fx.name
            );
        }
    }
}

/// The honest determinism finding, MEASURED not asserted. The evaluator runs a
/// stratum's rule heads through `rayon::par_iter` (eval.rs). `SimStorage`'s
/// fault plan keys off a global op-counter advanced under a mutex, and rayon
/// workers are not participants of the token-barrier scheduler — so for a
/// MULTI-HEAD stratum the counter→operation assignment races, and the
/// injected-fault targeting (hence the Ok/Err observable) can differ between
/// two runs of the SAME seed.
///
/// This test does not fail on divergence: it records the divergence rate so
/// the report can state precisely where query-level DST determinism holds and
/// where it needs an order-independent fault plan (the fix shape). If a future
/// change makes the multi-head path deterministic, `divergences` drops to 0
/// and the message documents that the hazard is closed.
#[test]
fn determinism_multihead_parallel_is_measured() {
    // Pin rayon to >1 thread so the race is actually reachable on CI hosts
    // with few cores; if the pool is already global, this is a no-op.
    let _ = rayon::ThreadPoolBuilder::new()
        .num_threads(4)
        .build_global();
    // 4%: recalibrated for the one-machine executor's denser read pattern
    // (see read_fault_campaign_correct_or_typed_never_wrong) — the assert
    // below demands BOTH observables, which keeps this rate honest.
    let faults = FaultConfig {
        read_fail_ppm: 40_000,
        ..Default::default()
    };
    let n = seeds(150);
    let mut divergences = 0u64;
    let mut oks = 0u64;
    let mut errs = 0u64;
    for seed in 0..n {
        let a = observe_faulted(&MULTIHEAD_FIXTURE, seed, faults);
        let b = observe_faulted(&MULTIHEAD_FIXTURE, seed, faults);
        if a != b {
            divergences += 1;
        }
        // The law that must hold REGARDLESS of determinism: whenever the run
        // completes, the answer is correct. Nondeterministic fault targeting
        // may flip Ok<->Err, but never Ok-with-a-wrong-answer.
        for obs in [&a, &b] {
            match obs {
                Observed::Answer(ans) => {
                    assert_eq!(
                        *ans,
                        (MULTIHEAD_FIXTURE.expected)(),
                        "CRITICAL: multihead returned a wrong answer under faults (seed {seed})"
                    );
                }
                Observed::TypedError => {}
            }
        }
        match a {
            Observed::Answer(_) => oks += 1,
            Observed::TypedError => errs += 1,
        }
    }
    // A meaningful determinism measurement needs BOTH observables present:
    // if every run errored (or every run completed), "0 diverged" would be
    // vacuous. Prove the probe actually straddles the Ok/Err boundary.
    assert!(
        oks > 0 && errs > 0,
        "determinism probe is vacuous: {oks} Ok / {errs} Err — need both arms present"
    );
    // Emit the measurement; a nonzero divergence rate is the documented
    // finding, not a failure. (Visible with `cargo test -- --nocapture`.)
    println!(
        "[dst-query] multihead determinism probe: {divergences}/{n} seeds diverged \
         run-to-run; observable split {oks} Ok / {errs} Err \
         (nonzero divergence ⇒ eval's rayon parallelism races the op-counter fault plan)"
    );
}

// ═════════════════════════════════════════════════════════════════════════
// Capability 6 — anti-vacuity: prove the assertions can fail.
// ═════════════════════════════════════════════════════════════════════════

/// Neuter the fault plan (ppm = 0) and confirm the fault machinery is what
/// makes the error arm fire: with no faults, EVERY seed completes with the
/// correct answer and NONE errors. If the error arm still fired here, the
/// campaign's "typed error" observations would be measuring something other
/// than injected faults.
#[test]
fn antivacuity_no_faults_means_no_errors() {
    let n = seeds(48);
    for fx in SINGLE_HEAD_FIXTURES {
        let expected = (fx.expected)();
        for seed in 0..n {
            match observe_faulted(fx, seed, FaultConfig::default()) {
                Observed::Answer(ans) => assert_eq!(
                    ans, expected,
                    "fixture '{}': fault-free run must be correct",
                    fx.name
                ),
                Observed::TypedError => panic!(
                    "fixture '{}' seed {seed}: a run errored with NO faults injected — \
                     the error arm is not measuring injected faults",
                    fx.name
                ),
            }
        }
    }
}

/// The fault machinery actually injects: at a high rate, the error arm fires
/// for a meaningful fraction of seeds. This is the "count injected faults,
/// assert nonzero" proof — if faults never reached the query, no seed would
/// error and the read-path campaign would be vacuous.
#[test]
fn antivacuity_faults_actually_inject() {
    let faults = FaultConfig {
        read_fail_ppm: 300_000,
        ..Default::default()
    };
    let n = seeds(64);
    let fx = &SINGLE_HEAD_FIXTURES[0]; // transitive closure: many scans
    let errs = (0..n)
        .filter(|&seed| matches!(observe_faulted(fx, seed, faults), Observed::TypedError))
        .count();
    assert!(
        errs as u64 > n / 10,
        "expected read faults to error many queries, but only {errs}/{n} errored — \
         the fault plan is not reaching the query path"
    );
}

/// The differential is not vacuously comparing a value to itself: a corrupted
/// reference is caught. We deliberately corrupt the expected set and confirm
/// the equality assertion the campaign relies on would reject it.
#[test]
fn antivacuity_corrupt_reference_is_caught() {
    let db = SimStorage::new(1);
    populate_retrying(&db, tc_populate).unwrap();
    let real = try_run(&db, tc_program()).unwrap();
    let mut corrupted = real.clone();
    corrupted.insert(rows(&[&[99, 99]]).into_iter().next().unwrap());
    assert_ne!(
        real, corrupted,
        "a corrupted reference must differ from the true answer — otherwise the \
         campaign's equality check could never catch a wrong answer"
    );
    // And the true answer equals the hand-checked oracle, closing the loop.
    assert_eq!(real, tc_expected());
}
