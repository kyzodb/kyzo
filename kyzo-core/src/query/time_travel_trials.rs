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
 * `data/tuple.rs::check_key_for_bitemporal` + the `ValidityTs::from_raw(_)`
 * ordering): an as-of read AT instant T is INCLUSIVE — it returns the newest
 * stored version whose real timestamp is <= T. At the same instant, an
 * assertion beats a retraction (assert encodes to byte 0x00, retract to 0x01,
 * and the seek lands on the assertion first). Two writes with an identical
 * (key, timestamp, is_assert) triple collapse by last-write-wins, because
 * they are the same key.
 */

#![cfg(test)]

use crate::engines::segments::Segments;
use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;

use smartstring::SmartString;

use crate::data::aggr::parse_aggr;
use crate::data::expr::Expr;
use crate::data::functions::{OP_GE, OP_LE};
use crate::data::program::{
    DeltaAxis, InputAtom, InputInlineRulesOrFixed, InputRelationHandle, MagicAtom, MagicInlineRule,
    MagicProgram, MagicRelationApplyAtom, MagicRuleApplyAtom, MagicRulesOrFixed, MagicSymbol,
    StoreLifetimes, StratifiedMagicProgram, ValidityClause,
};
use crate::data::relation::{ColType, ColumnDef, NullableColType, StoredRelationMetadata};
use crate::data::span::SourceSpan;
use crate::data::symb::Symbol;
use crate::data::value::{AsOf, Bound, DataValue, Interval, MAX_VALIDITY_TS, ValidityTs};
use crate::query::compile::{
    CompiledProgram, NoFixedRules, bind_for_eval, stratified_magic_compile,
};
use crate::query::eval::{Budget, RowLimit, stratified_evaluate};
use crate::query::laws;
use crate::query::ra::RelAlgebra;
use crate::query::ra::temporal;
use crate::runtime::relation::KeyspaceKind;
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
fn vts(ts: i64) -> ValidityTs {
    ValidityTs::from_raw(ts)
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
        validity: at.map(|t| ValidityClause::At(AsOf::current(vts(t)))),
        span: sp(),
    })
}
/// A negated stored-relation body atom, as-of `at` when given.
fn neg_rel_atom_at(name: &str, args: &[Symbol], at: Option<i64>) -> MagicAtom {
    MagicAtom::NegatedRelation(MagicRelationApplyAtom {
        name: sym(name),
        args: args.to_vec(),
        validity: at.map(|t| ValidityClause::At(AsOf::current(vts(t)))),
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
    let program = bind_for_eval::<_, NoFixedRules>(&compiled, &rtx, Segments::OFF, &mut |_| {
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

/// Create a versioned relation (`key_arity` integer key columns and
/// `val_names` non-key columns — the time slots are infrastructure, never
/// schema) and write every version at its valid instant: an assertion via
/// [`RelationHandle::put_fact`], a retraction via
/// [`RelationHandle::retract_fact`]. All versions land in ONE transaction
/// (one system stamp), so versions at the SAME (key, ts) share one stored
/// key and the LAST write in list order wins — the one-lineage-per-instant
/// law.
fn write_history(
    db: &FjallStorage,
    name: &str,
    key_arity: usize,
    val_names: &[&str],
    versions: &[Version],
) {
    let keys: Vec<ColumnDef> = (0..key_arity)
        .map(|i| col(&format!("k{i}"), ColType::Int))
        .collect();
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
    let handle = create_relation(&mut tx, input, KeyspaceKind::Facts).expect("create relation");
    for ver in versions {
        assert_eq!(ver.key.len(), key_arity, "version key arity");
        let key_cols: Tuple = ver.key.iter().copied().map(v).collect();
        if ver.assert {
            assert_eq!(ver.vals.len(), val_names.len(), "version value arity");
            let mut row = key_cols;
            row.extend(ver.vals.iter().cloned());
            handle
                .put_fact(&mut tx, &row, vts(ver.ts), sp())
                .expect("put version");
        } else {
            handle
                .retract_fact(&mut tx, &key_cols, vts(ver.ts), sp())
                .expect("retract version");
        }
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
    let handle = create_relation(&mut tx, input, KeyspaceKind::Facts).expect("create relation");
    for r in rows {
        let row: Tuple = r.iter().copied().map(v).collect();
        handle
            .put_fact(&mut tx, &row, ValidityTs::from_raw(0), sp())
            .expect("put row");
    }
    tx.commit().expect("commit");
}

/// The oracle: the file's `Version` history routed through the UNIFIED
/// temporal oracle (`query::laws::resolve_relation`) instead of a bespoke
/// collapse-then-group algorithm — story #62's oracle unification. Each
/// `Version` becomes a `laws::Event` at its own `(key, ts)` valid
/// coordinate; write order becomes the SYSTEM axis (`sys = list index`),
/// so "one lineage per instant, last write in write order governs" is
/// exactly `laws::resolve`'s "newest system version at or before `sys_at`
/// governs" with `sys_at` fixed at "see everything." `boundary_inclusive
/// = false` (probe `at - 1` instead of `at`, excluding the queried
/// instant) and `last_write_wins = false` (reverse the write-order→`sys`
/// encoding, so the FIRST write governs a collision) are the SABOTAGED
/// forms used only by the mutation tests below — still routed through
/// the one real resolution function, just fed a deliberately wrong
/// encoding of "which write governs." The correct engine behaviour is
/// `true` for both.
fn naive_asof_cfg(
    versions: &[Version],
    at: i64,
    boundary_inclusive: bool,
    last_write_wins: bool,
) -> BTreeSet<Tuple> {
    let n = versions.len() as i64;
    let events: Vec<laws::Event> = versions
        .iter()
        .enumerate()
        .map(|(i, ver)| {
            let key: Tuple = ver.key.iter().copied().map(v).collect();
            let sys = if last_write_wins {
                i as i64
            } else {
                n - 1 - i as i64
            };
            // `ver.ts` is a small, bounded generated/fixture timestamp
            // throughout this file: never the reserved terminal tick.
            if ver.assert {
                laws::Event::assert(key.into(), ver.vals.clone().into(), ver.ts, sys)
                    .expect("version timestamps in this file are never the reserved terminal tick")
            } else {
                laws::Event::retract(key.into(), ver.ts, sys)
                    .expect("version timestamps in this file are never the reserved terminal tick")
            }
        })
        .collect();
    let probe_at = if boundary_inclusive { at } else { at - 1 };
    laws::resolve_relation(
        &events,
        laws::AsOf {
            valid: probe_at,
            sys: i64::MAX,
        },
    )
    .into_iter()
    .collect()
}

/// The correct oracle: inclusive boundary, last-write-wins at an instant.
fn naive_asof(versions: &[Version], at: i64) -> BTreeSet<Tuple> {
    naive_asof_cfg(versions, at, true, true)
}

// ─────────────────────────────────────────────────────────────────────────
// Bridge differential (story #62): `naive_asof_cfg`'s unified-oracle
// encoding, checked against a FROM-SCRATCH reference — written without
// reusing any part of `naive_asof_cfg`, old or new — over hundreds of
// seeded random histories and all four (boundary, write-order) configs.
// ─────────────────────────────────────────────────────────────────────────

/// An independent brute-force reference: for every key, scan every
/// version linearly and keep the one the window-and-tiebreak rule picks
/// — same rule `naive_asof_cfg` states in its doc comment, computed here
/// by direct comparison instead of by building `laws::Event`s and calling
/// the unified oracle.
fn independent_asof_reference(
    versions: &[Version],
    at: i64,
    boundary_inclusive: bool,
    last_write_wins: bool,
) -> BTreeSet<Tuple> {
    let mut best: BTreeMap<Vec<i64>, (i64, usize, bool, Vec<DataValue>)> = BTreeMap::new();
    for (i, ver) in versions.iter().enumerate() {
        let in_window = if boundary_inclusive {
            ver.ts <= at
        } else {
            ver.ts < at
        };
        if !in_window {
            continue;
        }
        let candidate = (ver.ts, i, ver.assert, ver.vals.clone());
        match best.entry(ver.key.clone()) {
            std::collections::btree_map::Entry::Vacant(e) => {
                e.insert(candidate);
            }
            std::collections::btree_map::Entry::Occupied(mut e) => {
                let cur = e.get();
                let better = if candidate.0 != cur.0 {
                    candidate.0 > cur.0
                } else if last_write_wins {
                    candidate.1 > cur.1
                } else {
                    candidate.1 < cur.1
                };
                if better {
                    e.insert(candidate);
                }
            }
        }
    }
    let mut out = BTreeSet::new();
    for (key, (_, _, assert, vals)) in best {
        if assert {
            let mut row: Tuple = key.iter().copied().map(v).collect();
            row.extend(vals);
            out.insert(row);
        }
    }
    out
}

/// The splitmix64 generator of `query/trials.rs`, transcribed for this
/// file's own seeded campaign (one `u64` seed, replayable, no ambient
/// entropy).
struct BridgeRng {
    state: u64,
}
impl BridgeRng {
    fn new(seed: u64) -> Self {
        BridgeRng { state: seed }
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

/// A random version history over a handful of keys, small timestamps (so
/// same-instant collisions are common), and a mix of asserts/retracts.
fn gen_versions(rng: &mut BridgeRng, n_keys: i64, n_events: usize) -> Vec<Version> {
    (0..n_events)
        .map(|_| {
            let key = vec![rng.range(0, n_keys)];
            let ts = rng.range(0, 8);
            let assert = rng.chance(7, 10);
            let vals = if assert {
                vec![v(rng.range(0, 4))]
            } else {
                vec![]
            };
            ver(&key, ts, assert, &vals)
        })
        .collect()
}

/// The bridge: `naive_asof_cfg` (now backed by `laws::resolve_relation`)
/// against the from-scratch reference above, over hundreds of generated
/// histories, every probed instant, and all four boundary/write-order
/// configurations (the two sabotaged forms included — they must keep
/// agreeing with their own from-scratch counterpart, not just the
/// correct form with the correct one).
#[test]
fn naive_asof_cfg_matches_an_independent_reference_generatively() {
    let mut cases = 0usize;
    for seed in 0..300u64 {
        let mut rng = BridgeRng::new(0xA50F_0FFE_u64 ^ seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let n_keys = rng.range(1, 4);
        let n_events = rng.range(1, 20) as usize;
        let versions = gen_versions(&mut rng, n_keys, n_events);
        for at in -1..=9 {
            for boundary_inclusive in [true, false] {
                for last_write_wins in [true, false] {
                    let got = naive_asof_cfg(&versions, at, boundary_inclusive, last_write_wins);
                    let want = independent_asof_reference(
                        &versions,
                        at,
                        boundary_inclusive,
                        last_write_wins,
                    );
                    assert_eq!(
                        got, want,
                        "seed {seed} at={at} boundary_inclusive={boundary_inclusive} \
                         last_write_wins={last_write_wins}: versions={versions:?}"
                    );
                    cases += 1;
                }
            }
        }
    }
    assert!(cases > 500, "expected a rich bridge campaign, ran {cases}");
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

/// `?[k0..,v0..] := *rel{k0..,v0..} @ AT` — a bare projection scan. The
/// time slots are infrastructure: the row binds user columns only, and
/// the coordinate rides on the atom.
fn select_asof(key_arity: usize, val_names: &[&str], at: Option<i64>) -> StratifiedMagicProgram {
    let key_syms: Vec<Symbol> = (0..key_arity).map(|i| sym(&format!("k{i}"))).collect();
    let val_syms: Vec<Symbol> = val_names.iter().map(|n| sym(n)).collect();
    let mut args = key_syms.clone();
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

/// Same instant, assert then retract on the same key: ONE lineage per
/// instant, so the later write governs — polarity lives in the value and
/// there is no assert-vs-retract bucket order to tie-break. Both write
/// orders are pinned.
#[test]
fn same_instant_newest_write_governs() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    // Assert then retract: the retract is the instant's governing version.
    let hist = vec![
        ver(&[1], 10, true, &[s("present")]),
        ver(&[1], 10, false, &[]),
    ];
    write_history(&db, "hist", 1, &["val"], &hist);
    let got = compile_and_run(&db, select_asof(1, &["val"], Some(10)));
    assert!(got.is_empty(), "the later retract governs the instant");
    assert_eq!(got, naive_asof(&hist, 10));

    // Retract then assert: the assert governs.
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let hist = vec![
        ver(&[1], 10, false, &[]),
        ver(&[1], 10, true, &[s("present")]),
    ];
    write_history(&db, "hist", 1, &["val"], &hist);
    let got = compile_and_run(&db, select_asof(1, &["val"], Some(10)));
    assert_eq!(got, BTreeSet::from([vec![v(1), s("present")]]));
    assert_eq!(got, naive_asof(&hist, 10));
}

/// The tie-break law is differential-visible: a first-write-wins oracle
/// must DISAGREE with the engine on a polarity-flipping same-instant
/// history — else the harness is blind to the collapse rule.
#[test]
fn mutation_first_write_wins_is_caught() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let hist = vec![
        ver(&[1], 10, true, &[s("present")]),
        ver(&[1], 10, false, &[]),
    ];
    write_history(&db, "hist", 1, &["val"], &hist);
    let engine = compile_and_run(&db, select_asof(1, &["val"], Some(10)));
    let correct = naive_asof_cfg(&hist, 10, true, true);
    let sabotaged = naive_asof_cfg(&hist, 10, true, false);
    assert_eq!(engine, correct, "engine must match last-write-wins");
    assert_ne!(
        engine, sabotaged,
        "a first-write-wins oracle must disagree — else the harness is blind"
    );
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
        ver(&[7], 10, false, &[]),
        ver(&[7], 20, false, &[]),
        ver(&[7], 30, false, &[]),
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
        ver(&[1], 50, false, &[]),
        ver(&[1], 70, true, &[s("c")]),
        // key 2: assert@20 "p", retract@40 (latest fact for key 2)
        ver(&[2], 20, true, &[s("p")]),
        ver(&[2], 40, false, &[]),
        // key 3: assert@60 "q" only
        ver(&[3], 60, true, &[s("q")]),
        // key 4: assert and retract at the SAME instant — the later
        // write (the retract) governs the instant's one lineage.
        ver(&[4], 10, true, &[s("blink")]),
        ver(&[4], 10, false, &[]),
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

    let (x, y, z) = (sym("x"), sym("y"), sym("z"));
    let tc_program = |instant: i64| {
        program_of(vec![
            vec![(
                muggle("path"),
                vec![
                    plain_rule(
                        &[x.clone(), y.clone()],
                        vec![rel_atom_at("edge", &[x.clone(), y.clone()], Some(instant))],
                    ),
                    plain_rule(
                        &[x.clone(), y.clone()],
                        vec![
                            rel_atom_at("edge", &[x.clone(), z.clone()], Some(instant)),
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

    let (k, a, b) = (sym("k"), sym("a"), sym("b"));
    let join_program = |instant: i64| {
        program_of(vec![vec![(
            entry_symbol(),
            vec![plain_rule(
                &[k.clone(), a.clone(), b.clone()],
                vec![
                    MagicAtom::Relation(MagicRelationApplyAtom {
                        name: sym("ra"),
                        args: vec![k.clone(), a.clone()],
                        validity: Some(ValidityClause::At(AsOf::current(vts(instant)))),
                        span: sp(),
                    }),
                    MagicAtom::Relation(MagicRelationApplyAtom {
                        name: sym("rb"),
                        args: vec![k.clone(), b.clone()],
                        validity: Some(ValidityClause::At(AsOf::current(vts(instant)))),
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

    let (k0, val) = (sym("k0"), sym("val"));
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
                    &[k0.clone(), val.clone()],
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

    let (k0, val) = (sym("k0"), sym("val"));
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
                    &[k0.clone(), val.clone()],
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

    let (k0, k1, val) = (sym("k0"), sym("k1"), sym("val"));
    let bounded_program = |instant: i64| {
        program_of(vec![vec![(
            entry_symbol(),
            vec![plain_rule(
                &[k0.clone(), k1.clone(), val.clone()],
                vec![
                    rel_atom_at("drv", std::slice::from_ref(&k0), None),
                    rel_atom_at(
                        "hist",
                        &[k0.clone(), k1.clone(), val.clone()],
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

/// A PLAIN (coordinate-free) scan reads CURRENT state — the newest
/// believed claim per fact — never raw versions: the time slots are
/// infrastructure, and history is addressed through `@` coordinates, not
/// by enumerating stored rows. Retracted facts are absent; superseded
/// values are invisible.
#[test]
fn plain_scan_reads_current_state_not_versions() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let hist = vec![
        ver(&[1], 10, true, &[s("a")]),
        ver(&[1], 30, false, &[]),
        ver(&[2], 20, true, &[s("b")]),
        ver(&[3], 5, true, &[s("old")]),
        ver(&[3], 25, true, &[s("new")]),
    ];
    write_history(&db, "hist", 1, &["val"], &hist);

    let (k0, val) = (sym("k0"), sym("val"));
    let prog = program_of(vec![vec![(
        entry_symbol(),
        vec![plain_rule(
            &[k0.clone(), val.clone()],
            vec![rel_atom_at("hist", &[k0.clone(), val.clone()], None)],
        )],
    )]]);
    let got = compile_and_run(&db, prog);
    // Key 1 is retracted, key 2 asserted, key 3's newest value governs.
    let expected: BTreeSet<Tuple> = BTreeSet::from([vec![v(2), s("b")], vec![v(3), s("new")]]);
    assert_eq!(got, expected, "a plain scan is a current-state read");
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

    let (x, y, z) = (sym("x"), sym("y"), sym("z"));
    let build = || {
        program_of(vec![
            vec![(
                muggle("path"),
                vec![
                    plain_rule(
                        &[x.clone(), y.clone()],
                        vec![rel_atom_at("edge", &[x.clone(), y.clone()], Some(35))],
                    ),
                    plain_rule(
                        &[x.clone(), y.clone()],
                        vec![
                            rel_atom_at("edge", &[x.clone(), z.clone()], Some(35)),
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
// Task 5 — negation over validity scans (story #86: the skip-scan anti-join).
// ═════════════════════════════════════════════════════════════════════════

/// `?[k0,val] := *candidates{k0,val}, not *hist{k0,val}@AT` — the full-key
/// join, which compiles to `NegRight::StoredWithValidity`'s PREFIX-probe
/// branch (both `k0`, the real storage key, and `val`, a non-key payload
/// column, are already bound by the candidates atom, so
/// `join_is_prefix` sees a leading run `[0, 1]`). The candidate domain is
/// every `(key, val)` pair the generated history ever asserted, plus one
/// sentinel pair no history ever contains — rich enough that both
/// "present at AT" (negated out) and "absent at AT" (survives) rows occur.
/// Expected is computed independently: candidates minus [`naive_asof`]'s
/// own present set, never by re-deriving the engine's own answer.
#[test]
fn negation_over_asof_matches_the_independent_complement_generatively() {
    let mut cases = 0usize;
    for seed in 0..300u64 {
        let mut rng = BridgeRng::new(0x9E6A5F_u64.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ seed);
        let n_keys = rng.range(1, 4);
        let n_events = rng.range(1, 15) as usize;
        let hist = gen_versions(&mut rng, n_keys, n_events);

        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        write_history(&db, "neg_hist", 1, &["val"], &hist);

        let mut candidates: BTreeSet<(i64, i64)> = hist
            .iter()
            .filter(|ver| ver.assert)
            .map(|ver| (ver.key[0], ver.vals[0].get_int().unwrap()))
            .collect();
        candidates.insert((n_keys, 99));
        let candidate_rows: Vec<Vec<i64>> =
            candidates.iter().map(|&(k, val)| vec![k, val]).collect();
        stored_plain(&db, "neg_candidates", 2, &candidate_rows);

        let (k0, val) = (sym("k0"), sym("val"));
        for at in interesting_instants(&hist) {
            let prog = program_of(vec![vec![(
                entry_symbol(),
                vec![plain_rule(
                    &[k0.clone(), val.clone()],
                    vec![
                        rel_atom_at("neg_candidates", &[k0.clone(), val.clone()], None),
                        neg_rel_atom_at("neg_hist", &[k0.clone(), val.clone()], Some(at)),
                    ],
                )],
            )]]);
            let got = compile_and_run(&db, prog);
            let present = naive_asof(&hist, at);
            let want: BTreeSet<Tuple> = candidates
                .iter()
                .map(|&(k, val)| vec![v(k), v(val)])
                .filter(|row| !present.contains(row))
                .collect();
            assert_eq!(got, want, "seed {seed} at {at}: hist={hist:?}");
            cases += 1;
        }
    }
    assert!(
        cases >= 300,
        "expected a rich negation-over-as-of campaign, ran {cases}"
    );
}

/// The MATERIALIZED (non-prefix) branch: negating on `val` alone, with
/// `k0` left fresh (unjoined) on the negated side, so `right_join_indices`
/// is `[1]` — not a leading run of `hist`'s own columns — and
/// `NegRight::StoredWithValidity` must fall back to
/// `skip_scan_all`-then-set-membership instead of a prefix probe.
#[test]
fn negation_over_asof_non_prefix_join_matches_independent_complement() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    // Key 1's fact (value 100) is retracted at t=20 — chosen so that
    // querying at t=10 (BEFORE the retraction) and reading CURRENT state
    // (t=20+) disagree on whether 100 is present at all, discriminating
    // this branch from a same-shaped current-state probe.
    let hist = vec![
        ver(&[1], 10, true, &[v(100)]),
        ver(&[2], 10, true, &[v(200)]),
        ver(&[1], 20, false, &[]),
    ];
    write_history(&db, "nonprefix_hist", 1, &["val"], &hist);
    stored_plain(
        &db,
        "nonprefix_candidates",
        1,
        &[vec![100], vec![200], vec![999]],
    );

    let (dummy, val) = (sym("dummy"), sym("val"));
    // ?[val] := *nonprefix_candidates{val}, not *nonprefix_hist{dummy,val}@10
    let prog = program_of(vec![vec![(
        entry_symbol(),
        vec![plain_rule(
            std::slice::from_ref(&val),
            vec![
                rel_atom_at("nonprefix_candidates", std::slice::from_ref(&val), None),
                neg_rel_atom_at("nonprefix_hist", &[dummy, val.clone()], Some(10)),
            ],
        )],
    )]]);
    let got = compile_and_run(&db, prog);
    // At t=10, both 100 and 200 are present (the retraction has not
    // happened yet), so both are negated out; 999 is present nowhere, at
    // any instant.
    assert_eq!(got, BTreeSet::from([vec![v(999)]]));
}

/// Fixed rules reading stored relations (validity-keyed or not) go through
/// the `StoredInputSource` seam. In production `query/normalize.rs`'s
/// `SessionView` implements it for real, so a fixed rule consuming a
/// validity relation actually reads it. `NoStoredInputs` — exercised below
/// — is the superseded pre-runtime placeholder that refuses every stored
/// read with a typed, spanned `StoredInputUnavailable`; this test pins ONLY
/// that placeholder's own behavior, not current production semantics.
#[test]
fn fixed_rule_stored_input_is_a_refusing_seam() {
    use crate::fixed_rule::NoStoredInputs;
    use crate::fixed_rule::StoredInputSource;

    let src = NoStoredInputs;
    // As-of read of a (would-be) validity relation refuses, typed.
    let err = src
        .stored_scan_all(&sym("hist"), Some(AsOf::current(vts(20))))
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
        vec![sym("k0"), sym("val")],
        handle,
        sp(),
        Some(ValidityClause::At(AsOf::current(vts(20)))),
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
        ver(&[1], 30, false, &[]), // the retraction the engine will see
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

/// The two-coordinate as-of read, end to end: a correction (a second
/// system version at the same valid instant) is invisible at system
/// coordinates before its stamp and governs at coordinates from its
/// stamp on. This is the bitemporal flagship: "what did the record say
/// at S about V?"
#[test]
fn two_coordinate_asof_sees_the_record_as_it_was() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();

    // The relation and the original claim, in one transaction (stamp s1).
    let mut tx = db.write_tx().unwrap();
    let handle = create_relation(
        &mut tx,
        {
            let keys = vec![col("k0", ColType::Int)];
            let non_keys = vec![col("val", ColType::Any)];
            let key_bindings = vec![sym("k0")];
            let dep_bindings = vec![sym("val")];
            InputRelationHandle {
                name: sym("hist"),
                metadata: StoredRelationMetadata { keys, non_keys },
                key_bindings,
                dep_bindings,
                span: sp(),
            }
        },
        KeyspaceKind::Facts,
    )
    .unwrap();
    let s1 = tx.system_stamp();
    handle
        .put_fact(&mut tx, &[v(1), s("original")], vts(100), sp())
        .unwrap();
    tx.commit().unwrap();

    // The correction: a second system version of the SAME valid instant
    // (stamp s2 > s1).
    let mut tx = db.write_tx().unwrap();
    let s2 = tx.system_stamp();
    assert!(
        s2 < s1,
        "stamps are monotone (Reverse order: later is smaller)"
    );
    handle
        .put_fact(&mut tx, &[v(1), s("corrected")], vts(100), sp())
        .unwrap();
    tx.commit().unwrap();

    let read_at = |as_of: AsOf| -> BTreeSet<Tuple> {
        let (k0, val) = (sym("k0"), sym("val"));
        let prog = program_of(vec![vec![(
            entry_symbol(),
            vec![plain_rule(
                &[k0.clone(), val.clone()],
                vec![MagicAtom::Relation(MagicRelationApplyAtom {
                    name: sym("hist"),
                    args: vec![k0, val],
                    validity: Some(ValidityClause::At(as_of)),
                    span: sp(),
                })],
            )],
        )]]);
        compile_and_run(&db, prog)
    };

    // As the record stood at s1: the original claim.
    assert_eq!(
        read_at(AsOf {
            sys: s1,
            valid: vts(150)
        }),
        BTreeSet::from([vec![v(1), s("original")]]),
        "before the correction was recorded, the original governs"
    );
    // As the record stands at s2 (and currently): the correction.
    assert_eq!(
        read_at(AsOf {
            sys: s2,
            valid: vts(150)
        }),
        BTreeSet::from([vec![v(1), s("corrected")]]),
        "from the correction's stamp on, it governs"
    );
    assert_eq!(
        read_at(AsOf::current(vts(150))),
        BTreeSet::from([vec![v(1), s("corrected")]]),
        "current belief is the corrected claim"
    );
    // Valid-axis still resolves under both system cuts.
    assert!(
        read_at(AsOf {
            sys: s2,
            valid: vts(50)
        })
        .is_empty(),
        "before the valid instant, the fact does not hold at any system cut"
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

// ─────────────────────────────────────────────────────────────────────────
// Story #62 chunk 3: `@spans`/`@delta`/`@delta_sys` end to end — real
// storage, real compile → RA → semi-naive eval, `query::laws`'s
// `derive_intervals`/`diff` as judge. Follows this file's own structure:
// `write_history`-style fact injection, `program_of`/`plain_rule`-style
// program construction, `compile_and_run` as the engine path.
// ─────────────────────────────────────────────────────────────────────────

/// Like [`write_history`], but ONE transaction per version — every version
/// gets a genuinely distinct, monotonically increasing REAL system stamp
/// (the engine's own clock, `WriteTx::system_stamp`), returned in write
/// order. Interval derivation and diff both read real system stamps
/// (`SpansRA`'s fixed snapshot, `DeltaRA`'s two coordinates), so the
/// oracle side of this file's differentials must be built from the exact
/// stamps the engine actually minted, not a synthetic index — unlike
/// [`write_history`]'s single-transaction batch (adequate for as-of reads
/// at the CURRENT system snapshot, where every version's real stamp is
/// equally visible regardless of relative order).
fn write_history_multi_tx(
    db: &FjallStorage,
    name: &str,
    key_arity: usize,
    val_names: &[&str],
    versions: &[Version],
) -> Vec<i64> {
    let keys: Vec<ColumnDef> = (0..key_arity)
        .map(|i| col(&format!("k{i}"), ColType::Int))
        .collect();
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
    let handle = {
        let mut tx = db.write_tx().expect("write tx");
        let handle = create_relation(&mut tx, input, KeyspaceKind::Facts).expect("create relation");
        tx.commit().expect("commit");
        handle
    };
    let mut sys_stamps = Vec::with_capacity(versions.len());
    for ver in versions {
        let mut tx = db.write_tx().expect("write tx");
        sys_stamps.push(tx.system_stamp().raw());
        let key_cols: Tuple = ver.key.iter().copied().map(v).collect();
        if ver.assert {
            let mut row = key_cols;
            row.extend(ver.vals.iter().cloned());
            handle
                .put_fact(&mut tx, &row, vts(ver.ts), sp())
                .expect("put version");
        } else {
            handle
                .retract_fact(&mut tx, &key_cols, vts(ver.ts), sp())
                .expect("retract version");
        }
        tx.commit().expect("commit");
    }
    sys_stamps
}

/// `versions` + their real system stamps (from
/// [`write_history_multi_tx`]), as `laws::Event`s — the exact bridge
/// between this file's `Version` fixture shape and the oracle's own
/// history type, sys taken from the engine's real clock rather than a
/// synthetic write-order index.
fn events_of(versions: &[Version], sys_stamps: &[i64]) -> Vec<laws::Event> {
    assert_eq!(versions.len(), sys_stamps.len());
    versions
        .iter()
        .zip(sys_stamps.iter())
        .map(|(ver, &sys)| {
            let key: Tuple = ver.key.iter().copied().map(v).collect();
            if ver.assert {
                laws::Event::assert(key.into(), ver.vals.clone().into(), ver.ts, sys)
            } else {
                laws::Event::retract(key.into(), ver.ts, sys)
            }
            .expect("fixture valid instants are never the reserved terminal tick")
        })
        .collect()
}

fn distinct_keys(versions: &[Version]) -> Vec<Tuple> {
    let mut ks: Vec<Tuple> = versions
        .iter()
        .map(|ver| ver.key.iter().copied().map(v).collect())
        .collect();
    ks.sort();
    ks.dedup();
    ks
}

/// `*rel{args...} @spans iv[, sys]` as a `MagicAtom`.
fn spans_atom(name: &str, args: &[Symbol], sys: Option<i64>, iv: Symbol) -> MagicAtom {
    MagicAtom::Relation(MagicRelationApplyAtom {
        name: sym(name),
        args: args.to_vec(),
        validity: Some(ValidityClause::Spans {
            sys: sys.map(vts).unwrap_or(MAX_VALIDITY_TS),
            var: iv,
        }),
        span: sp(),
    })
}

/// `*rel{args...} @delta(from, to) sgn` / `@delta_sys(from, to) sgn` as a
/// `MagicAtom`.
fn delta_atom(
    name: &str,
    args: &[Symbol],
    axis: DeltaAxis,
    from: i64,
    to: i64,
    sgn: Symbol,
) -> MagicAtom {
    MagicAtom::Relation(MagicRelationApplyAtom {
        name: sym(name),
        args: args.to_vec(),
        validity: Some(ValidityClause::Delta {
            axis,
            from: vts(from),
            to: vts(to),
            var: sgn,
        }),
        span: sp(),
    })
}

/// The oracle's derived intervals for every fact key in `events`, at fixed
/// system snapshot `fixed_sys`, as engine-shaped rows (`key ++ payload ++
/// Interval`) — what `SpansRA`'s output must equal exactly.
fn oracle_spans(events: &[laws::Event], keys: &[Tuple], fixed_sys: i64) -> BTreeSet<Tuple> {
    let mut want = BTreeSet::new();
    for key in keys {
        let real_key = crate::data::value::Tuple::from(key.clone());
        for iv in laws::derive_intervals(events, &real_key, laws::Axis::Valid, fixed_sys) {
            let mut row = iv.tuple.clone();
            row.push(DataValue::Interval(Interval::new(
                Bound::Closed(iv.start),
                Bound::Closed(iv.end),
            )));
            want.insert(row.to_vec());
        }
    }
    want
}

/// The oracle's signed diff, as engine-shaped rows (`key ++ payload ++
/// sign`) — what `DeltaRA`'s output must equal exactly (order-independent;
/// both sides are compared as sets).
fn oracle_delta(events: &[laws::Event], from: laws::AsOf, to: laws::AsOf) -> BTreeSet<Tuple> {
    laws::diff(events, from, to)
        .into_iter()
        .map(|sf| match sf {
            laws::SignedFact::Plus(mut t) => {
                t.push(v(1));
                t
            }
            laws::SignedFact::Minus(mut t) => {
                t.push(v(-1));
                t
            }
        })
        .collect()
}

#[test]
fn spans_engine_matches_the_unified_oracle_generatively() {
    let mut cases = 0usize;
    for seed in 0..300u64 {
        let mut rng = BridgeRng::new(0x5FA5FA_u64.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ seed);
        let n_keys = rng.range(1, 4);
        let n_events = rng.range(1, 15) as usize;
        let versions = gen_versions(&mut rng, n_keys, n_events);

        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let sys_stamps = write_history_multi_tx(&db, "spans_src", 1, &["val"], &versions);
        let events = events_of(&versions, &sys_stamps);
        let keys = distinct_keys(&versions);

        let (k0, val, iv) = (sym("k0"), sym("val"), sym("iv"));
        for fixed_sys in [None, Some(sys_stamps[sys_stamps.len() / 2])] {
            let prog = program_of(vec![vec![(
                entry_symbol(),
                vec![plain_rule(
                    &[k0.clone(), val.clone(), iv.clone()],
                    vec![spans_atom(
                        "spans_src",
                        &[k0.clone(), val.clone()],
                        fixed_sys,
                        iv.clone(),
                    )],
                )],
            )]]);
            let got = compile_and_run(&db, prog);
            let want = oracle_spans(&events, &keys, fixed_sys.unwrap_or(i64::MAX));
            assert_eq!(
                got, want,
                "seed {seed} fixed_sys={fixed_sys:?}: versions={versions:?} sys_stamps={sys_stamps:?}"
            );
            cases += 1;
        }
    }
    assert!(cases >= 300, "expected a rich spans campaign, ran {cases}");
}

/// Story #62 ruling item 3 ("as-of over a derived relation pushes down to
/// that subtree's stored leaves — the only compositional reading"):
/// verified as a PROPERTY of this design rather than built as a separate
/// feature. Both the oracle and the engine refuse a coordinate anywhere
/// but a literal reading a stored/historical relation directly
/// (`laws.rs::check_wellformed`'s "`as_of` valid only if
/// `histories.contains_key(lit.rel)`" invariant; mirrored here by
/// `MagicRuleApplyAtom`/`NormalFormRuleApplyAtom`/`InputRuleApplyAtom`
/// never gaining a `validity` field alongside `MagicRelationApplyAtom`'s)
/// — so "push to stored leaves" is DEFINITIONAL: the only place a clause
/// can ever be written already IS the leaf. What remains to prove is that
/// composition actually works: an inner rule wraps a `@spans` read of a
/// stored leaf; an outer rule joins/filters the inner rule's output — two
/// levels of ordinary rule nesting over a temporal read, composing
/// exactly like any other relation.
#[test]
fn spans_composes_through_ordinary_rule_nesting() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    write_history_multi_tx(
        &db,
        "sp_compose_src",
        1,
        &["val"],
        &[
            ver(&[1], 10, true, &[v(100)]),
            ver(&[1], 20, true, &[v(200)]),
        ],
    );

    let (k0, val, iv) = (sym("k0"), sym("val"), sym("iv"));
    let inner_rule = plain_rule(
        &[k0.clone(), val.clone(), iv.clone()],
        vec![spans_atom(
            "sp_compose_src",
            &[k0.clone(), val.clone()],
            None,
            iv.clone(),
        )],
    );
    let outer_rule = plain_rule(
        &[k0.clone(), val.clone(), iv.clone()],
        vec![
            MagicAtom::Rule(MagicRuleApplyAtom {
                name: muggle("sp_inner"),
                args: vec![k0.clone(), val.clone(), iv.clone()],
                span: sp(),
            }),
            pred_ge("val", 150),
        ],
    );
    let prog = program_of(vec![
        vec![(muggle("sp_inner"), vec![inner_rule])],
        vec![(entry_symbol(), vec![outer_rule])],
    ]);
    let got = compile_and_run(&db, prog);
    assert_eq!(
        got,
        BTreeSet::from([one_interval(1, 200, 20, i64::MAX)]),
        "the outer rule's filter must see the inner rule's derived interval row"
    );
}

#[test]
fn delta_engine_matches_the_unified_oracle_generatively_both_axes() {
    let mut cases = 0usize;
    for seed in 0..300u64 {
        let mut rng = BridgeRng::new(0xDE17AD_u64.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ seed);
        let n_keys = rng.range(1, 4);
        let n_events = rng.range(1, 15) as usize;
        let versions = gen_versions(&mut rng, n_keys, n_events);

        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let sys_stamps = write_history_multi_tx(&db, "delta_src", 1, &["val"], &versions);
        let events = events_of(&versions, &sys_stamps);

        let (k0, val, sgn) = (sym("k0"), sym("val"), sym("sgn"));
        for valid_at in -1..=9i64 {
            for valid_to in -1..=9i64 {
                let prog = program_of(vec![vec![(
                    entry_symbol(),
                    vec![plain_rule(
                        &[k0.clone(), val.clone(), sgn.clone()],
                        vec![delta_atom(
                            "delta_src",
                            &[k0.clone(), val.clone()],
                            DeltaAxis::Valid,
                            valid_at,
                            valid_to,
                            sgn.clone(),
                        )],
                    )],
                )]]);
                let got = compile_and_run(&db, prog);
                let want = oracle_delta(
                    &events,
                    laws::AsOf {
                        valid: valid_at,
                        sys: i64::MAX,
                    },
                    laws::AsOf {
                        valid: valid_to,
                        sys: i64::MAX,
                    },
                );
                assert_eq!(
                    got, want,
                    "seed {seed} valid axis {valid_at}->{valid_to}: versions={versions:?}"
                );
                cases += 1;
            }
        }
        let sys_lo = sys_stamps.iter().copied().min().unwrap_or(0) - 1;
        let sys_hi = sys_stamps.iter().copied().max().unwrap_or(0) + 1;
        for sys_at in [sys_lo, sys_stamps[sys_stamps.len() / 2], sys_hi] {
            for sys_to in [sys_lo, sys_stamps[sys_stamps.len() / 2], sys_hi] {
                let prog = program_of(vec![vec![(
                    entry_symbol(),
                    vec![plain_rule(
                        &[k0.clone(), val.clone(), sgn.clone()],
                        vec![delta_atom(
                            "delta_src",
                            &[k0.clone(), val.clone()],
                            DeltaAxis::Sys,
                            sys_at,
                            sys_to,
                            sgn.clone(),
                        )],
                    )],
                )]]);
                let got = compile_and_run(&db, prog);
                let want = oracle_delta(
                    &events,
                    laws::AsOf {
                        valid: i64::MAX,
                        sys: sys_at,
                    },
                    laws::AsOf {
                        valid: i64::MAX,
                        sys: sys_to,
                    },
                );
                assert_eq!(
                    got, want,
                    "seed {seed} sys axis {sys_at}->{sys_to}: versions={versions:?} sys_stamps={sys_stamps:?}"
                );
                cases += 1;
            }
        }
    }
    assert!(cases >= 300, "expected a rich delta campaign, ran {cases}");
}

/// `diff(a,c) == diff(a,b) ⊕ diff(b,c)` through the REAL engine: three
/// independent `DeltaRA` evaluations, composed by `laws::compose` (the
/// executable law), must equal the direct `a->c` evaluation. Spot-checks
/// the compositionality law end to end rather than only inside the oracle
/// (already proven there, `laws.rs`'s own tests).
#[test]
fn delta_composition_law_holds_through_the_real_engine() {
    for seed in 0..80u64 {
        let mut rng = BridgeRng::new(0xC02EC0_u64.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ seed);
        let n_keys = rng.range(1, 3);
        let n_events = rng.range(2, 12) as usize;
        let versions = gen_versions(&mut rng, n_keys, n_events);

        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        write_history_multi_tx(&db, "compose_src", 1, &["val"], &versions);

        let (k0, val, sgn) = (sym("k0"), sym("val"), sym("sgn"));
        let run_delta = |from: i64, to: i64| -> BTreeSet<Tuple> {
            let prog = program_of(vec![vec![(
                entry_symbol(),
                vec![plain_rule(
                    &[k0.clone(), val.clone(), sgn.clone()],
                    vec![delta_atom(
                        "compose_src",
                        &[k0.clone(), val.clone()],
                        DeltaAxis::Valid,
                        from,
                        to,
                        sgn.clone(),
                    )],
                )],
            )]]);
            compile_and_run(&db, prog)
        };
        let to_signed = |rows: BTreeSet<Tuple>| -> BTreeSet<laws::SignedFact> {
            rows.into_iter()
                .map(|mut row| {
                    let sgn = row.pop().expect("row carries a sign column");
                    match sgn.get_int() {
                        Some(1) => laws::SignedFact::Plus(row.into()),
                        Some(-1) => laws::SignedFact::Minus(row.into()),
                        other => panic!("unexpected sign column: {other:?}"),
                    }
                })
                .collect()
        };
        for (a, b, c) in [(-1, 3, 9), (0, 0, 5), (2, 2, 2), (-1, 9, -1)] {
            let ab = to_signed(run_delta(a, b));
            let bc = to_signed(run_delta(b, c));
            let ac = to_signed(run_delta(a, c));
            let composed = laws::compose(&ab, &bc);
            assert_eq!(
                composed, ac,
                "seed {seed} a={a} b={b} c={c}: diff(a,c) != diff(a,b)⊕diff(b,c)"
            );
        }
    }
}

/// Story #77: `query/ra/temporal.rs`'s PRODUCTION `SignedFact`/`compose` —
/// not just the oracle's, which the test above already closes — proven
/// against real engine output. Before this story `compose` was "proven as
/// an oracle law but wired into zero production code"; this is the
/// production copy's own differential, independently written from the
/// oracle's (never sharing a bug through shared code) and checked the same
/// way: three real `DeltaRA` evaluations, composed by the PRODUCTION
/// function, must equal the direct evaluation.
#[test]
fn production_compose_matches_the_composition_law_on_real_engine_output() {
    for seed in 0..80u64 {
        let mut rng = BridgeRng::new(0xC0DE_u64.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ seed);
        let n_keys = rng.range(1, 3);
        let n_events = rng.range(2, 12) as usize;
        let versions = gen_versions(&mut rng, n_keys, n_events);

        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        write_history_multi_tx(&db, "prod_compose_src", 1, &["val"], &versions);

        let (k0, val, sgn) = (sym("k0"), sym("val"), sym("sgn"));
        let run_delta = |from: i64, to: i64| -> BTreeSet<Tuple> {
            let prog = program_of(vec![vec![(
                entry_symbol(),
                vec![plain_rule(
                    &[k0.clone(), val.clone(), sgn.clone()],
                    vec![delta_atom(
                        "prod_compose_src",
                        &[k0.clone(), val.clone()],
                        DeltaAxis::Valid,
                        from,
                        to,
                        sgn.clone(),
                    )],
                )],
            )]]);
            compile_and_run(&db, prog)
        };
        let to_signed = |rows: BTreeSet<Tuple>| -> BTreeSet<temporal::SignedFact> {
            rows.into_iter()
                .map(|mut row| {
                    let sgn = row.pop().expect("row carries a sign column");
                    match sgn.get_int() {
                        Some(1) => temporal::SignedFact::Plus(row.into()),
                        Some(-1) => temporal::SignedFact::Minus(row.into()),
                        other => panic!("unexpected sign column: {other:?}"),
                    }
                })
                .collect()
        };
        for (a, b, c) in [(-1, 3, 9), (0, 0, 5), (2, 2, 2), (-1, 9, -1)] {
            let ab = to_signed(run_delta(a, b));
            let bc = to_signed(run_delta(b, c));
            let ac = to_signed(run_delta(a, c));
            let composed = temporal::compose(&ab, &bc);
            assert_eq!(
                composed, ac,
                "seed {seed} a={a} b={b} c={c}: production compose diverged from diff(a,c)"
            );
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Degenerate cases from the ruling's table, named individually (each also
// covered by the generative campaigns above, but pinned here by name so a
// regression fails with a readable label rather than only a seed).
// ─────────────────────────────────────────────────────────────────────────

fn spans_of(db: &FjallStorage, name: &str, sys: Option<i64>) -> BTreeSet<Tuple> {
    let (k0, val, iv) = (sym("k0"), sym("val"), sym("iv"));
    let prog = program_of(vec![vec![(
        entry_symbol(),
        vec![plain_rule(
            &[k0.clone(), val.clone(), iv.clone()],
            vec![spans_atom(
                name,
                &[k0.clone(), val.clone()],
                sys,
                iv.clone(),
            )],
        )],
    )]]);
    compile_and_run(db, prog)
}

fn one_interval(k: i64, val: i64, start: i64, end: i64) -> Tuple {
    vec![
        v(k),
        v(val),
        DataValue::Interval(Interval::new(Bound::Closed(start), Bound::Closed(end))),
    ]
}

#[test]
fn spans_double_assert_same_payload_is_idempotent_one_interval() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    write_history_multi_tx(
        &db,
        "sp_double_assert",
        1,
        &["val"],
        &[
            ver(&[1], 10, true, &[v(100)]),
            ver(&[1], 20, true, &[v(100)]),
        ],
    );
    assert_eq!(
        spans_of(&db, "sp_double_assert", None),
        BTreeSet::from([one_interval(1, 100, 10, i64::MAX)])
    );
}

#[test]
fn spans_payload_split_produces_two_intervals() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    write_history_multi_tx(
        &db,
        "sp_payload_split",
        1,
        &["val"],
        &[
            ver(&[1], 10, true, &[v(100)]),
            ver(&[1], 20, true, &[v(200)]),
        ],
    );
    assert_eq!(
        spans_of(&db, "sp_payload_split", None),
        BTreeSet::from([
            one_interval(1, 100, 10, 20),
            one_interval(1, 200, 20, i64::MAX)
        ])
    );
}

#[test]
fn spans_dangling_retract_holds_nowhere() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    write_history_multi_tx(
        &db,
        "sp_dangling_retract",
        1,
        &["val"],
        &[ver(&[1], 10, false, &[])],
    );
    assert_eq!(spans_of(&db, "sp_dangling_retract", None), BTreeSet::new());
}

#[test]
fn spans_assert_after_retract_opens_a_new_interval() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    write_history_multi_tx(
        &db,
        "sp_reopen",
        1,
        &["val"],
        &[
            ver(&[1], 10, true, &[v(100)]),
            ver(&[1], 20, false, &[]),
            ver(&[1], 30, true, &[v(100)]),
        ],
    );
    assert_eq!(
        spans_of(&db, "sp_reopen", None),
        BTreeSet::from([
            one_interval(1, 100, 10, 20),
            one_interval(1, 100, 30, i64::MAX)
        ])
    );
}

#[test]
fn spans_no_zero_width_intervals_ever() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    write_history_multi_tx(
        &db,
        "sp_no_zero_width",
        1,
        &["val"],
        &[
            ver(&[1], 10, true, &[v(100)]),
            ver(&[1], 10, true, &[v(200)]),
        ],
    );
    for row in spans_of(&db, "sp_no_zero_width", None) {
        let DataValue::Interval(iv) = row.last().unwrap() else {
            panic!("expected an interval column: {row:?}")
        };
        assert!(iv.start() < iv.end(), "zero-width interval: {iv:?}");
    }
}

#[test]
fn spans_at_write_op_is_refused_at_parse() {
    // `@spans`/`@delta`/`@delta_sys` ride the read-side grammar seat only
    // (`read_validity_clause`, referenced from `relation_apply`/
    // `relation_named_apply`); `relation_option` (the write-op clause)
    // still references plain `validity_clause` alone, so this is a hard
    // grammar-level refusal — the construct is unparseable, not merely
    // semantically rejected.
    let err = crate::parse::parse_script(
        ":create sp_write_refused {k: Int, val: Any} @spans iv",
        &Default::default(),
        &Default::default(),
        vts(0),
    )
    .expect_err("`@spans` on a write op must not parse");
    let msg = err.to_string();
    assert!(
        !msg.is_empty(),
        "expected a parse error for `@spans` on a write op"
    );
}

#[test]
fn validity_clause_on_a_rule_application_is_refused_at_parse() {
    // `rule_apply` (`ident[args…]`, no `*` sigil) has no `@`/`@spans`/
    // `@delta` production at all — the ONLY grammar seat for any
    // validity/temporal clause is a stored-relation atom
    // (`relation_apply`/`relation_named_apply`, both `*`-sigiled). A
    // coordinate on a rule application is therefore unparseable, not
    // merely semantically refused — confirming story #62 ruling item 3
    // ("as-of over IDB pushes to stored leaves") is structural: there is
    // no OTHER place to write one.
    let err = crate::parse::parse_script(
        "r[x] := *base_rel[x]; ?[x] := r[x] @ 5",
        &Default::default(),
        &Default::default(),
        vts(0),
    )
    .expect_err("`@` on a rule application must not parse");
    assert!(!err.to_string().is_empty());
}

// ─────────────────────────────────────────────────────────────────────────
// Textual parse coverage for `@spans`/`@delta`/`@delta_sys` (hostile-review
// finding: every other test in this file builds `MagicAtom`s directly,
// never exercising `kyzoscript.pest`/`parse::query.rs` themselves — the
// keyword-boundary bug below lived in exactly that unexercised seam).
// ─────────────────────────────────────────────────────────────────────────

/// Parse `script` and return the entry rule's first body atom's
/// `InputRelationApplyAtom`/`InputNamedFieldRelationApplyAtom` validity
/// clause — the shape both positive and refusal tests below inspect.
fn parsed_validity_clause(script: &str) -> ValidityClause {
    let prog = crate::parse::parse_script(script, &Default::default(), &Default::default(), vts(0))
        .expect("expected a successful parse")
        .get_single_program()
        .expect("single program");
    let InputInlineRulesOrFixed::Rules { rules } = prog.entry() else {
        panic!("expected an inline-rule entry")
    };
    let atom = rules[0].body.first().expect("one body atom");
    match atom {
        InputAtom::Relation { inner } => inner
            .validity
            .clone()
            .expect("expected a validity clause on the atom"),
        other => panic!("expected a stored-relation atom, got {other:?}"),
    }
}

#[test]
fn spans_clause_parses_through_real_script_text() {
    match parsed_validity_clause("?[x, v, iv] := *base_rel[x, v @spans iv]") {
        ValidityClause::Spans { sys, var } => {
            assert_eq!(sys, MAX_VALIDITY_TS, "default sys is the current snapshot");
            assert_eq!(var.name, "iv");
        }
        other => panic!("expected a Spans clause, got {other:?}"),
    }
}

#[test]
fn spans_clause_with_explicit_sys_parses_through_real_script_text() {
    match parsed_validity_clause("?[x, v, iv] := *base_rel[x, v @spans iv, 5]") {
        ValidityClause::Spans { sys, var } => {
            assert_eq!(sys, vts(5));
            assert_eq!(var.name, "iv");
        }
        other => panic!("expected a Spans clause, got {other:?}"),
    }
}

#[test]
fn delta_clause_parses_through_real_script_text() {
    match parsed_validity_clause("?[x, v, sgn] := *base_rel[x, v @delta(0, 10) sgn]") {
        ValidityClause::Delta {
            axis,
            from,
            to,
            var,
        } => {
            assert_eq!(axis, DeltaAxis::Valid);
            assert_eq!(from, vts(0));
            assert_eq!(to, vts(10));
            assert_eq!(var.name, "sgn");
        }
        other => panic!("expected a Delta clause, got {other:?}"),
    }
}

#[test]
fn delta_sys_clause_parses_through_real_script_text() {
    match parsed_validity_clause("?[x, v, sgn] := *base_rel[x, v @delta_sys(0, 10) sgn]") {
        ValidityClause::Delta {
            axis,
            from,
            to,
            var,
        } => {
            assert_eq!(axis, DeltaAxis::Sys);
            assert_eq!(from, vts(0));
            assert_eq!(to, vts(10));
            assert_eq!(var.name, "sgn");
        }
        other => panic!("expected a Delta clause, got {other:?}"),
    }
}

/// Hostile-review finding (CONFIRMED, reproduced): `spans_clause`'s
/// `"@spans"` literal had no keyword-boundary guard, so `@spansX` parsed
/// as `@spans` with `X` silently bound as the interval variable —
/// `kyzoscript.pest`'s own convention for `not`/`in`/`or` is `"kw" ~
/// !XID_CONTINUE`, which `@spans`/`@delta`/`@delta_sys` now all carry
/// too. With the guard, `@spansX` falls through to the byte-identical
/// `@ expr` clause, where `spansX` is a free variable — refused, because
/// a read-side `@` coordinate must be a compile-time constant
/// (`parse_at_expr_clause`'s `eval_to_const`). This is the test a mutant
/// reverting the guard must fail: without it, this parses successfully
/// (as a working-but-wrong `@spans` derivation) instead of refusing.
#[test]
fn spans_keyword_requires_a_boundary_not_just_a_prefix_match() {
    let err = crate::parse::parse_script(
        "?[x, v] := *base_rel[x, v @spansX]",
        &Default::default(),
        &Default::default(),
        vts(0),
    )
    .expect_err("`@spansX` must not silently parse as `@spans` with `X` as its interval variable");
    assert!(!err.to_string().is_empty());
}

#[test]
fn delta_keyword_requires_a_boundary_not_just_a_prefix_match() {
    let err = crate::parse::parse_script(
        "?[x, v] := *base_rel[x, v @deltafoo]",
        &Default::default(),
        &Default::default(),
        vts(0),
    )
    .expect_err("`@deltafoo` must not silently parse as `@delta` plus a mistyped tail");
    assert!(!err.to_string().is_empty());
}

#[test]
fn delta_sys_keyword_requires_a_boundary_not_just_a_prefix_match() {
    let err = crate::parse::parse_script(
        "?[x, v] := *base_rel[x, v @delta_sysX]",
        &Default::default(),
        &Default::default(),
        vts(0),
    )
    .expect_err("`@delta_sysX` must not silently parse as `@delta_sys` plus a mistyped tail");
    assert!(!err.to_string().is_empty());
}

/// `not *rel{args...} @spans iv` as a `MagicAtom` — [`spans_atom`]'s
/// negated twin (story #86: no longer a refusal).
fn neg_spans_atom(name: &str, args: &[Symbol], sys: Option<i64>, iv: Symbol) -> MagicAtom {
    MagicAtom::NegatedRelation(MagicRelationApplyAtom {
        name: sym(name),
        args: args.to_vec(),
        validity: Some(ValidityClause::Spans {
            sys: sys.map(vts).unwrap_or(MAX_VALIDITY_TS),
            var: iv,
        }),
        span: sp(),
    })
}

/// `not *rel{args...} @delta(from, to) sgn` as a `MagicAtom` —
/// [`delta_atom`]'s negated twin (story #86).
fn neg_delta_atom(
    name: &str,
    args: &[Symbol],
    axis: DeltaAxis,
    from: i64,
    to: i64,
    sgn: Symbol,
) -> MagicAtom {
    MagicAtom::NegatedRelation(MagicRelationApplyAtom {
        name: sym(name),
        args: args.to_vec(),
        validity: Some(ValidityClause::Delta {
            axis,
            from: vts(from),
            to: vts(to),
            var: sgn,
        }),
        span: sp(),
    })
}

/// Negating `@spans` now computes (story #86: `NegRight::Spans`), through
/// the SAME materialized-scan primitive the positive read uses
/// (`SpansRA::iter_batched`). `iv` is left unjoined in the negated atom (a
/// fresh var, discarded like any right-only column never selected as a
/// join key), so the negation asks "does `(k0,val)` govern SOME interval",
/// independently checked by projecting `oracle_spans`'s own rows onto
/// `(k0,val)` and set-differencing against a candidate domain.
#[test]
fn negation_over_spans_matches_the_independent_complement_generatively() {
    let mut cases = 0usize;
    for seed in 0..150u64 {
        let mut rng = BridgeRng::new(0xB16_5FA5_u64.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ seed);
        let n_keys = rng.range(1, 4);
        let n_events = rng.range(1, 15) as usize;
        let versions = gen_versions(&mut rng, n_keys, n_events);

        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let sys_stamps = write_history_multi_tx(&db, "neg_spans_src", 1, &["val"], &versions);
        let events = events_of(&versions, &sys_stamps);
        let keys = distinct_keys(&versions);

        let (k0, val, iv) = (sym("k0"), sym("val"), sym("iv"));
        for (variant, fixed_sys) in [None, Some(sys_stamps[sys_stamps.len() / 2])]
            .into_iter()
            .enumerate()
        {
            let positive = oracle_spans(&events, &keys, fixed_sys.unwrap_or(i64::MAX));
            let present_pairs: BTreeSet<(i64, i64)> = positive
                .iter()
                .map(|row| (row[0].get_int().unwrap(), row[1].get_int().unwrap()))
                .collect();
            let mut candidates = present_pairs.clone();
            candidates.insert((n_keys, 99));
            let candidate_rows: Vec<Vec<i64>> =
                candidates.iter().map(|&(k, val)| vec![k, val]).collect();
            // A fresh relation name per variant: candidates get re-derived
            // every iteration (they depend on `fixed_sys`), and a name
            // cannot be created twice in the same `db`.
            let candidates_name = format!("neg_spans_candidates_{variant}");
            stored_plain(&db, &candidates_name, 2, &candidate_rows);

            let prog = program_of(vec![vec![(
                entry_symbol(),
                vec![plain_rule(
                    &[k0.clone(), val.clone()],
                    vec![
                        rel_atom_at(&candidates_name, &[k0.clone(), val.clone()], None),
                        neg_spans_atom(
                            "neg_spans_src",
                            &[k0.clone(), val.clone()],
                            fixed_sys,
                            iv.clone(),
                        ),
                    ],
                )],
            )]]);
            let got = compile_and_run(&db, prog);
            let want: BTreeSet<Tuple> = candidates
                .iter()
                .map(|&(k, val)| vec![v(k), v(val)])
                .filter(|row| !present_pairs.contains(&(k0_of(row), val_of(row))))
                .collect();
            assert_eq!(
                got, want,
                "seed {seed} fixed_sys={fixed_sys:?}: versions={versions:?}"
            );
            cases += 1;
        }
    }
    assert!(
        cases >= 150,
        "expected a rich negation-over-spans campaign, ran {cases}"
    );
}

/// Negating `@delta` now computes (story #86: `NegRight::Delta`), through
/// the same materialized set-difference the positive `DeltaRA` uses. `sgn`
/// is left unjoined, so the negation asks "does `(k0,val)` appear as EITHER
/// sign in the diff" — checked against `oracle_delta`'s own rows projected
/// onto `(k0,val)`.
#[test]
fn negation_over_delta_matches_the_independent_complement_generatively() {
    let mut cases = 0usize;
    for seed in 0..150u64 {
        let mut rng = BridgeRng::new(0xDE17ADBADu64.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ seed);
        let n_keys = rng.range(1, 4);
        let n_events = rng.range(1, 15) as usize;
        let versions = gen_versions(&mut rng, n_keys, n_events);

        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let sys_stamps = write_history_multi_tx(&db, "neg_delta_src", 1, &["val"], &versions);
        let events = events_of(&versions, &sys_stamps);

        let (k0, val, sgn) = (sym("k0"), sym("val"), sym("sgn"));
        for (variant, (valid_at, valid_to)) in [(-1i64, 9i64), (0, 5), (3, 3), (9, -1)]
            .into_iter()
            .enumerate()
        {
            let positive = oracle_delta(
                &events,
                laws::AsOf {
                    valid: valid_at,
                    sys: i64::MAX,
                },
                laws::AsOf {
                    valid: valid_to,
                    sys: i64::MAX,
                },
            );
            let present_pairs: BTreeSet<(i64, i64)> = positive
                .iter()
                .map(|row| (row[0].get_int().unwrap(), row[1].get_int().unwrap()))
                .collect();
            let mut candidates = present_pairs.clone();
            candidates.insert((n_keys, 99));
            let candidate_rows: Vec<Vec<i64>> =
                candidates.iter().map(|&(k, val)| vec![k, val]).collect();
            // A fresh relation name per variant — see the spans campaign's
            // matching comment above `candidates_name`.
            let candidates_name = format!("neg_delta_candidates_{variant}");
            stored_plain(&db, &candidates_name, 2, &candidate_rows);

            let prog = program_of(vec![vec![(
                entry_symbol(),
                vec![plain_rule(
                    &[k0.clone(), val.clone()],
                    vec![
                        rel_atom_at(&candidates_name, &[k0.clone(), val.clone()], None),
                        neg_delta_atom(
                            "neg_delta_src",
                            &[k0.clone(), val.clone()],
                            DeltaAxis::Valid,
                            valid_at,
                            valid_to,
                            sgn.clone(),
                        ),
                    ],
                )],
            )]]);
            let got = compile_and_run(&db, prog);
            let want: BTreeSet<Tuple> = candidates
                .iter()
                .map(|&(k, val)| vec![v(k), v(val)])
                .filter(|row| !present_pairs.contains(&(k0_of(row), val_of(row))))
                .collect();
            assert_eq!(
                got, want,
                "seed {seed} valid axis {valid_at}->{valid_to}: versions={versions:?}"
            );
            cases += 1;
        }
    }
    assert!(
        cases >= 150,
        "expected a rich negation-over-delta campaign, ran {cases}"
    );
}

fn k0_of(row: &[DataValue]) -> i64 {
    row[0].get_int().unwrap()
}
fn val_of(row: &[DataValue]) -> i64 {
    row[1].get_int().unwrap()
}
