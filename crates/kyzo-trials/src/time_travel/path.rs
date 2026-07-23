/*
 * Copyright 2023 The Cozo Project Authors.
 * Copyright 2026 The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License,
 * v. 2.0. If a copy of the MPL was not distributed with this file, You can
 * obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Capabilities 3–4 — temporal generator twin + refusal-lift coverage.
//!
//! Relocated from condemned `kyzo-core::query::trials`. Oracle-vs-oracle
//! generative coverage over `kyzo_oracle::temporal` / `naive_eval_at`.
//!
//! ## Epistemics (preserved from trials.rs)
//!
//! (a) `resolve`'s direct point resolution against `derive_intervals`'s
//! interval reconstruction are genuinely TWO independent algorithms over
//! the same events, so the grid differentials resolution correctness itself.
//!
//! (b) `diff`/`compose`'s compositionality law is a mathematical identity
//! over `diff`'s own outputs, not an independence claim.
//!
//! (c) `naive_eval_at`'s per-literal coordinate pushdown is checked against
//! `resolve_relation` called directly per coordinate then hand-joined —
//! this proves the PUSHDOWN WIRING (each literal occurrence resolves at
//! its OWN coordinate), not `resolve_relation`'s resolution algebra,
//! which (a) already covers.
//!
//! DEVIATION: the prior Fjall full-path as-of battery that occupied this
//! seat is not adapted here (`crate::` in-tree + engines/storage surfaces).
//! Cap3–4 from `query/trials.rs` own the seat per the cut destiny.

#![cfg(test)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;


/// Fail the trial loudly — `assert!` is always live (not `debug_assert`).
#[cfg(test)]
fn must_ok<T, E: std::fmt::Display>(r: Result<T, E>, ctx: &str) -> T {
    match r {
        Ok(v) => v,
        Err(e) => loop {
            assert!(false, "{ctx}: {e}");
        },
    }
}

#[cfg(test)]
fn must_some<T>(o: Option<T>, ctx: &str) -> T {
    match o {
        Some(v) => v,
        None => loop {
            assert!(false, "{ctx}");
        },
    }
}

#[cfg(test)]
pub(crate) fn mix_seed(seed: u64) -> u64 {
    // Modular × golden-ratio constant in Z/2^64Z (splitmix diffusion).
    let wide = u128::from(seed) * u128::from(0x9E37_79B9_7F4A_7C15u64);
    let low = wide & u128::from(u64::MAX);
    match u64::try_from(low) {
        Ok(v) => v,
        Err(_) => loop {
            assert!(false, "low 64 bits always fit u64");
        },
    }
}

#[cfg(test)]
fn u64_as_i64(n: u64) -> i64 {
    match i64::try_from(n) {
        Ok(v) => v,
        Err(_) => loop {
            assert!(false, "u64->i64 overflow");
        },
    }
}

#[cfg(test)]
fn u64_as_usize(n: u64) -> usize {
    match usize::try_from(n) {
        Ok(v) => v,
        Err(_) => loop {
            assert!(false, "u64->usize overflow");
        },
    }
}

#[cfg(test)]
fn usize_as_i64(n: usize) -> i64 {
    match i64::try_from(n) {
        Ok(v) => v,
        Err(_) => loop {
            assert!(false, "usize->i64 overflow");
        },
    }
}

#[cfg(test)]
fn i64_as_usize(n: i64) -> usize {
    match usize::try_from(n) {
        Ok(v) => v,
        Err(_) => loop {
            assert!(false, "i64->usize overflow");
        },
    }
}

fn sat_add_i64(a: i64, b: i64) -> i64 {
    match a.checked_add(b) {
        Some(v) => v,
        None if b > 0 => {
            // Published saturating climb for harness time coords.
            i64::MAX
        }
        None => {
            i64::MIN
        }
    }
}


use kyzo_model::value::{DataValue, Tuple};
use kyzo_oracle::{
    AggrFold, AsOf, Axis, ClaimPolarity, Event, HeadAggr, Interval, Literal, MeetAccum, OPEN_END,
    Program, Rel, Rule, Term, builtin_fold, compose, derive_intervals, diff, naive_eval,
    naive_eval_at, resolve, resolve_relation,
};

use crate::gauntlet::{MEET_OPS, Rng, lit, named, v, x, y, z};

// ════════════════════════════════════════════════════════════════════════
// CAPABILITY 3 — sys-axis generative coverage: the unified temporal
// oracle's OWN internal consistency, generatively.
// ════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone)]
struct TemporalGenParams {
    n_relations: usize,
    keys_per_relation: i64,
    events_per_key: i64,
    coord_span: i64,
}

fn gen_temporal_params(rng: &mut Rng) -> TemporalGenParams {
    TemporalGenParams {
        n_relations: i64_as_usize(rng.range(1, 4)),
        keys_per_relation: rng.range(1, 4),
        events_per_key: rng.range(2, 8),
        coord_span: rng.range(4, 20),
    }
}

const TEMPORAL_POLARITIES: [ClaimPolarity; 3] = [
    ClaimPolarity::Assert,
    ClaimPolarity::Retract,
    ClaimPolarity::Erase,
];

/// Coordinate domain for temporal history generation.
#[derive(Clone, Copy)]
enum CoordDomain {
    /// Full signed span — the campaign seat (negative coords load-bearing).
    Signed,
    /// Non-negative only — mutant that blinds abs-sort campaigns.
    NonNeg,
}

fn gen_temporal_history_in(
    rng: &mut Rng,
    key: &Tuple,
    p: &TemporalGenParams,
    domain: CoordDomain,
) -> Vec<Event> {
    let mut history = Vec::new();
    for _ in 0..p.events_per_key {
        let (valid, sys) = match domain {
            CoordDomain::Signed => (
                rng.range(-p.coord_span, p.coord_span),
                rng.range(-p.coord_span, p.coord_span),
            ),
            CoordDomain::NonNeg => (
                rng.range(0, p.coord_span.max(1)),
                rng.range(0, p.coord_span.max(1)),
            ),
        };
        push_temporal_event(&mut history, rng, key, valid, sys);
        if rng.chance(2, 5) {
            let correction_sys = sys + rng.range(1, 5);
            push_temporal_event(&mut history, rng, key, valid, correction_sys);
        }
    }
    history
}

fn gen_temporal_history(rng: &mut Rng, key: &Tuple, p: &TemporalGenParams) -> Vec<Event> {
    gen_temporal_history_in(rng, key, p, CoordDomain::Signed)
}

fn push_temporal_event(history: &mut Vec<Event>, rng: &mut Rng, key: &Tuple, valid: i64, sys: i64) {
    let event = match rng.one_of(&TEMPORAL_POLARITIES) {
        ClaimPolarity::Assert => Event::assert(
            key.clone(),
            Tuple::from_vec(vec![v(rng.range(0, 5))]),
            valid,
            sys,
        ),
        ClaimPolarity::Retract => Event::retract(key.clone(), valid, sys),
        ClaimPolarity::Erase => Event::erase(key.clone(), valid, sys),
    };
    history.push(must_ok(event, "coord_span keeps every draw far below the reserved terminal tick"));
}

struct TemporalHistories {
    per_relation: BTreeMap<Rel, BTreeMap<Tuple, Vec<Event>>>,
}

/// Relation names in a fixed order so seed reproducibility never depends
/// on `BTreeMap` iteration order deciding which subset gets used.
const HIST_RELS: [&str; 3] = ["ha", "hb", "hc"];

fn gen_temporal_histories(rng: &mut Rng, p: &TemporalGenParams) -> TemporalHistories {
    let mut per_relation = BTreeMap::new();
    for &rel in HIST_RELS.iter().take(p.n_relations) {
        let mut per_key = BTreeMap::new();
        for i in 0..p.keys_per_relation {
            let key: Tuple = Tuple::from_vec(vec![v(i)]);
            per_key.insert(key.clone(), gen_temporal_history(rng, &key, p));
        }
        per_relation.insert(rel.into(), per_key);
    }
    TemporalHistories { per_relation }
}

impl TemporalHistories {
    fn flat(&self, rel: &Rel) -> Vec<Event> {
        self.per_relation[rel].values().flatten().cloned().collect()
    }
}

/// Permutes a generated rule's body literals in place (Fisher-Yates) —
/// Mutant-C lineage: hunts body-order sensitivity that
/// positives-before-negatives emission would mask.
fn shuffle_body(rng: &mut Rng, body: &mut [Literal]) {
    for i in (1..body.len()).rev() {
        let j = u64_as_usize(rng.below(must_ok(u64::try_from(i + 1), "shuffle idx")));
        body.swap(i, j);
    }
}

fn temporal_program(rng: &mut Rng, hist: &TemporalHistories) -> Program {
    let mut histories: BTreeMap<Rel, Vec<Event>> = BTreeMap::new();
    for rel in hist.per_relation.keys() {
        histories.insert(rel.clone(), hist.flat(rel));
    }
    let mut rules: Vec<Rule> = histories
        .keys()
        .map(|rel| {
            Rule::plain(
                "out",
                vec![x(), y()],
                vec![lit(rel.clone(), vec![x(), y()], false)],
            )
        })
        .collect();
    if histories.contains_key("ha") && histories.contains_key("hb") {
        rules.push(Rule::plain(
            "joined",
            vec![x(), y(), z()],
            vec![
                lit("ha", vec![x(), y()], false),
                lit("hb", vec![x(), z()], false),
            ],
        ));
    }
    for rule in &mut rules {
        shuffle_body(rng, &mut rule.body);
    }
    Program {
        rules,
        histories,
        ..Program::empty()
    }
}

/// Every distinct stored coordinate of `history` on `axis`, ± one tick,
/// plus the extremes — complete-grid pattern for generated histories.
fn program_grid(history: &[Event], axis: Axis) -> Vec<i64> {
    let mut pts: Vec<i64> = history
        .iter()
        .flat_map(|e| {
            let c = match axis {
                Axis::Valid => e.valid(),
                Axis::Sys => e.sys(),
            };
            [c - 1, c, c + 1]
        })
        .collect();
    pts.push(i64::MIN);
    // Not `i64::MAX` itself: it is `OPEN_END`/`AsOf::current()`'s shared
    // sentinel, never a real stored coordinate.
    pts.push(i64::MAX - 1);
    pts.sort_unstable();
    pts.dedup();
    pts
}

#[test]
fn grid_differential_over_generated_temporal_programs() {
    let mut cases = 0usize;
    let seeds = 400u64;
    for seed in 0..seeds {
        // INVARIANT(test_seed_mix): property-test seed diffusion uses modular golden mix.
        let mut rng = Rng::new(0x7E57_A105_u64 ^ mix_seed(seed));
        let params = gen_temporal_params(&mut rng);
        let hist = gen_temporal_histories(&mut rng, &params);
        let program = temporal_program(&mut rng, &hist);

        for (rel, history) in &program.histories {
            let keys: BTreeSet<&Tuple> = history.iter().map(|e| e.key()).collect();
            for key in keys {
                let valid_grid = program_grid(history, Axis::Valid);
                let sys_grid = program_grid(history, Axis::Sys);
                for &sys_pt in &sys_grid {
                    let ivs = derive_intervals(history, key, Axis::Valid, sys_pt);
                    for &valid_pt in &valid_grid {
                        let direct = resolve(
                            history,
                            key,
                            AsOf {
                                valid: valid_pt,
                                sys: sys_pt,
                            },
                        );
                        let via_intervals = ivs
                            .iter()
                            .find(|iv| iv.start <= valid_pt && valid_pt < iv.end)
                            .map(|iv| iv.tuple.clone());
                        assert_eq!(
                            direct, via_intervals,
                            "seed {seed} rel={rel} key={key:?}: valid axis \
                             valid={valid_pt} sys={sys_pt}"
                        );
                        cases += 1;
                    }
                }
                for &fixed_valid in &[match history.first() {
                        Some(e) => e.valid(),
                        None => {
                            // Empty history — valid epoch 0.
                            0
                        }
                    }, 0] {
                    let ivs = derive_intervals(history, key, Axis::Sys, fixed_valid);
                    for &sys_pt in &sys_grid {
                        let direct = resolve(
                            history,
                            key,
                            AsOf {
                                valid: fixed_valid,
                                sys: sys_pt,
                            },
                        );
                        let via_intervals = ivs
                            .iter()
                            .find(|iv| iv.start <= sys_pt && sys_pt < iv.end)
                            .map(|iv| iv.tuple.clone());
                        assert_eq!(
                            direct, via_intervals,
                            "seed {seed} rel={rel} key={key:?}: sys axis \
                             fixed_valid={fixed_valid} sys={sys_pt}"
                        );
                        cases += 1;
                    }
                }
            }
        }

        let db = must_ok(naive_eval_at(&program, AsOf::current()), "well-formed generated program");
        let mut expected_out: BTreeSet<Tuple> = BTreeSet::new();
        for history in program.histories.values() {
            expected_out.extend(resolve_relation(history, AsOf::current()));
        }
        assert_eq!(
            match db.get("out") { Some(s) => s.clone(), None => BTreeSet::new() },
            expected_out,
            "seed {seed}: union wiring"
        );
        cases += 1;
        if let (Some(ha), Some(hb)) = (program.histories.get("ha"), program.histories.get("hb")) {
            let snap_a = resolve_relation(ha, AsOf::current());
            let snap_b = resolve_relation(hb, AsOf::current());
            let mut expected_joined: BTreeSet<Tuple> = BTreeSet::new();
            for row_a in &snap_a {
                for row_b in &snap_b {
                    if row_a[0] == row_b[0] {
                        expected_joined.insert(Tuple::from_vec(vec![
                            row_a[0].clone(),
                            row_a[1].clone(),
                            row_b[1].clone(),
                        ]));
                    }
                }
            }
            assert_eq!(
                match db.get("joined") { Some(s) => s.clone(), None => BTreeSet::new() },
                expected_joined,
                "seed {seed}: join wiring"
            );
            cases += 1;
        }
    }
    assert!(
        cases > 5000,
        "expected a rich grid campaign over generated programs, ran {cases}"
    );
}

fn ordered_triple(rng: &mut Rng, span: i64) -> (i64, i64, i64) {
    let lo = -(span * 2) - 5;
    let hi = span * 2 + 5;
    loop {
        let mut xs = [rng.range(lo, hi), rng.range(lo, hi), rng.range(lo, hi)];
        xs.sort_unstable();
        if xs[0] < xs[1] && xs[1] < xs[2] {
            return (xs[0], xs[1], xs[2]);
        }
    }
}

#[test]
fn diff_composition_law_holds_with_randomized_bounds_over_generated_histories() {
    let mut cases = 0usize;
    let seeds = 400u64;
    for seed in 0..seeds {
        // INVARIANT(test_seed_mix): property-test seed diffusion uses modular golden mix.
        let mut rng = Rng::new(0xD1FF_5EED_u64 ^ mix_seed(seed));
        let params = gen_temporal_params(&mut rng);
        let key: Tuple = Tuple::from_vec(vec![v(0)]);
        let history = gen_temporal_history(&mut rng, &key, &params);

        let sys_now = AsOf::current().sys;
        let (av, bv, cv) = ordered_triple(&mut rng, params.coord_span);
        let a = AsOf {
            valid: av,
            sys: sys_now,
        };
        let b = AsOf {
            valid: bv,
            sys: sys_now,
        };
        let c = AsOf {
            valid: cv,
            sys: sys_now,
        };
        let ab = diff(&history, a, b);
        let bc = diff(&history, b, c);
        let ac = diff(&history, a, c);
        assert_eq!(
            must_ok(compose(&ab, &bc), "unit net"),
            ac,
            "seed {seed}: valid axis a={av} b={bv} c={cv}"
        );
        cases += 1;

        let fixed_valid = match history.first() {
                        Some(e) => e.valid(),
                        None => {
                            // Empty history — valid epoch 0.
                            0
                        }
                    };
        let (asys, bsys, csys) = ordered_triple(&mut rng, params.coord_span);
        let a = AsOf {
            valid: fixed_valid,
            sys: asys,
        };
        let b = AsOf {
            valid: fixed_valid,
            sys: bsys,
        };
        let c = AsOf {
            valid: fixed_valid,
            sys: csys,
        };
        let ab = diff(&history, a, b);
        let bc = diff(&history, b, c);
        let ac = diff(&history, a, c);
        assert_eq!(
            must_ok(compose(&ab, &bc), "unit net"),
            ac,
            "seed {seed}: sys axis a={asys} b={bsys} c={csys}"
        );
        cases += 1;
    }
    assert!(
        cases >= 500,
        "expected hundreds of randomized-bounds composition cases, ran {cases}"
    );
}

fn lit_at(rel: impl Into<Rel>, args: Vec<Term>, at: AsOf) -> Literal {
    Literal::pos_at(rel, args, at)
}

fn near_coordinate(rng: &mut Rng, history: &[Event]) -> AsOf {
    if history.is_empty() {
        return AsOf { valid: 0, sys: 0 };
    }
    let events: Vec<&Event> = history.iter().collect();
    let e = rng.one_of(&events);
    AsOf {
        valid: nudge(rng, e.valid()),
        sys: nudge(rng, e.sys()),
    }
}

fn nudge(rng: &mut Rng, coordinate: i64) -> i64 {
    let out = sat_add_i64(coordinate, rng.range(-2, 3));
    if out == i64::MAX { out - 1 } else { out }
}

#[test]
fn per_literal_asof_pushdown_matches_independent_single_coordinate_resolution() {
    let mut cases = 0usize;
    let seeds = 400u64;
    for seed in 0..seeds {
        // INVARIANT(test_seed_mix): property-test seed diffusion uses modular golden mix.
        let mut rng = Rng::new(0x9A5D_6E1B_u64 ^ mix_seed(seed));
        let params = gen_temporal_params(&mut rng);
        let mut history = Vec::new();
        for i in 0..params.keys_per_relation {
            history.extend(gen_temporal_history(
                &mut rng,
                &Tuple::from_vec(vec![v(i)]),
                &params,
            ));
        }

        let c1 = near_coordinate(&mut rng, &history);
        let c2 = near_coordinate(&mut rng, &history);

        let mut histories: BTreeMap<Rel, Vec<Event>> = BTreeMap::new();
        histories.insert("hx".into(), history);
        let program = Program {
            rules: vec![Rule::plain(
                "out",
                vec![x(), y(), z()],
                vec![
                    lit_at("hx", vec![x(), y()], c1),
                    lit_at("hx", vec![x(), z()], c2),
                ],
            )],
            histories,
            ..Program::empty()
        };

        let got = must_ok(naive_eval(&program), "well-formed generated program")
            .get("out");

        let hx = &program.histories["hx"];
        let snap1 = resolve_relation(hx, c1);
        let snap2 = resolve_relation(hx, c2);
        let mut expected: BTreeSet<Tuple> = BTreeSet::new();
        for row1 in &snap1 {
            for row2 in &snap2 {
                if row1[0] == row2[0] {
                    expected.insert(Tuple::from_vec(vec![
                        row1[0].clone(),
                        row1[1].clone(),
                        row2[1].clone(),
                    ]));
                }
            }
        }
        assert_eq!(got, expected, "seed {seed}: c1={c1:?} c2={c2:?}");
        cases += 1;
    }
    assert!(
        cases >= 300,
        "expected hundreds of pushdown-consistency cases, ran {cases}"
    );
}

// ── Hand mutants (three pairs) ───────────────────────────────────────────

fn resolve_erase_as_retract_bug(history: &[Event], key: &Tuple, at: AsOf) -> Option<Tuple> {
    let mut instants: Vec<i64> = history
        .iter()
        .filter(|e| e.key() == key && e.valid() <= at.valid)
        .map(|e| e.valid())
        .collect();
    instants.sort_unstable();
    instants.dedup();
    for instant in instants.into_iter().rev() {
        let governing = history
            .iter()
            .filter(|e| e.key() == key && e.valid() == instant && e.sys() <= at.sys)
            .max_by_key(|e| e.sys());
        match governing {
            Some(Event::Assert {
                key: k, payload, ..
            }) => {
                let mut t = k.clone();
                t.extend(payload.iter().cloned());
                return Some(t);
            }
            Some(Event::Retract { .. }) | Some(Event::Erase { .. }) => return None,
            None => {}
        }
    }
    None
}

fn gen_temporal_history_no_erase(rng: &mut Rng, key: &Tuple, p: &TemporalGenParams) -> Vec<Event> {
    let mut history = Vec::new();
    for _ in 0..p.events_per_key {
        let valid = rng.range(-p.coord_span, p.coord_span);
        let sys = rng.range(-p.coord_span, p.coord_span);
        let event = if rng.chance(1, 2) {
            Event::assert(
                key.clone(),
                Tuple::from_vec(vec![v(rng.range(0, 5))]),
                valid,
                sys,
            )
        } else {
            Event::retract(key.clone(), valid, sys)
        };
        history
            .push(must_ok(event, "coord_span keeps every draw far below the reserved terminal tick"));
        if rng.chance(2, 5) {
            let correction_sys = sys + rng.range(1, 5);
            history.push(
                must_ok(Event::assert(
                    key.clone(),
                    Tuple::from_vec(vec![v(rng.range(0, 5))]),
                    valid,
                    correction_sys,
                ), "coord_span keeps every draw far below the reserved terminal tick"),
            );
        }
    }
    history
}

fn erase_bug_manifests(history: &[Event], key: &Tuple) -> bool {
    for &valid in &program_grid(history, Axis::Valid) {
        for &sys in &program_grid(history, Axis::Sys) {
            let at = AsOf { valid, sys };
            if resolve(history, key, at) != resolve_erase_as_retract_bug(history, key, at) {
                return true;
            }
        }
    }
    false
}

/// Mutant campaign: does `generate` expose `manifests` across `seeds` from `seed_mix`?
fn campaign_catches_bug(
    seed_mix: u64,
    seeds: u64,
    key: &Tuple,
    generate: impl Fn(&mut Rng, &Tuple, &TemporalGenParams) -> Vec<Event>,
    manifests: impl Fn(&[Event], &Tuple) -> bool,
) -> bool {
    let mut caught = false;
    for seed in 0..seeds {
        // INVARIANT(test_seed_mix): property-test seed diffusion uses modular golden mix.
        let mut rng = Rng::new(seed_mix ^ mix_seed(seed));
        let params = gen_temporal_params(&mut rng);
        let history = generate(&mut rng, key, &params);
        caught |= manifests(&history, key);
    }
    caught
}

#[test]
fn mutant_dropping_erase_from_generation_blinds_the_campaign() {
    let seeds = 300u64;
    let key: Tuple = Tuple::from_vec(vec![v(0)]);
    let mix = 0xE1A5_E000_u64;

    assert!(
        !campaign_catches_bug(mix, seeds, &key, gen_temporal_history_no_erase, erase_bug_manifests),
        "without Erase in generation, the erase-mishandling bug is structurally unreachable"
    );
    assert!(
        campaign_catches_bug(mix, seeds, &key, gen_temporal_history, erase_bug_manifests),
        "the real generator (with Erase) must expose the erase-mishandling bug"
    );
}

/// Deliberate interval-derivation mutants — one door, two independent faults.
#[derive(Clone, Copy)]
enum IntervalBug {
    /// Sort breaks by `|coord|` instead of signed order.
    AbsSort,
    /// End a closed interval one tick early (`breaks[j+1] - 1`).
    ShortEnd,
}

fn derive_intervals_bug(
    history: &[Event],
    key: &Tuple,
    axis: Axis,
    fixed: i64,
    bug: IntervalBug,
) -> Vec<Interval> {
    let mut breaks: Vec<i64> = history
        .iter()
        .filter(|e| e.key() == key)
        .map(|e| match axis {
            Axis::Valid => e.valid(),
            Axis::Sys => e.sys(),
        })
        .collect();
    match bug {
        IntervalBug::AbsSort => breaks.sort_unstable_by_key(|b| b.unsigned_abs()),
        IntervalBug::ShortEnd => breaks.sort_unstable(),
    }
    breaks.dedup();
    let coordinate = |pt: i64| -> AsOf {
        match axis {
            Axis::Valid => AsOf {
                valid: pt,
                sys: fixed,
            },
            Axis::Sys => AsOf {
                valid: fixed,
                sys: pt,
            },
        }
    };
    let mut out = Vec::new();
    let mut i = 0;
    while i < breaks.len() {
        let start = breaks[i];
        let Some(tuple) = resolve(history, key, coordinate(start)) else {
            i += 1;
            continue;
        };
        let mut j = i;
        while j + 1 < breaks.len()
            && resolve(history, key, coordinate(breaks[j + 1])) == Some(tuple.clone())
        {
            j += 1;
        }
        let end = if j + 1 < breaks.len() {
            match bug {
                IntervalBug::AbsSort => breaks[j + 1],
                IntervalBug::ShortEnd => breaks[j + 1] - 1,
            }
        } else {
            OPEN_END
        };
        out.push(Interval { start, end, tuple });
        i = j + 1;
    }
    out
}

fn interval_bug_manifests(history: &[Event], key: &Tuple, grid: &[i64], bug: IntervalBug) -> bool {
    let ivs = derive_intervals_bug(history, key, Axis::Valid, AsOf::current().sys, bug);
    for &valid in grid {
        let at = AsOf {
            valid,
            sys: AsOf::current().sys,
        };
        let direct = resolve(history, key, at);
        let via = ivs
            .iter()
            .find(|iv| iv.start <= valid && valid < iv.end)
            .map(|iv| iv.tuple.clone());
        if direct != via {
            return true;
        }
    }
    false
}

fn gen_temporal_history_nonneg(rng: &mut Rng, key: &Tuple, p: &TemporalGenParams) -> Vec<Event> {
    gen_temporal_history_in(rng, key, p, CoordDomain::NonNeg)
}

fn abs_sort_bug_manifests(history: &[Event], key: &Tuple) -> bool {
    interval_bug_manifests(
        history,
        key,
        &program_grid(history, Axis::Valid),
        IntervalBug::AbsSort,
    )
}

#[test]
fn mutant_skipping_negative_coordinates_blinds_the_campaign() {
    let seeds = 300u64;
    let key: Tuple = Tuple::from_vec(vec![v(0)]);
    let mix = 0xA65_5169_u64;

    assert!(!campaign_catches_bug(
        mix,
        seeds,
        &key,
        gen_temporal_history_nonneg,
        abs_sort_bug_manifests,
    ));
    assert!(campaign_catches_bug(
        mix,
        seeds,
        &key,
        gen_temporal_history,
        abs_sort_bug_manifests,
    ));
}

fn short_end_bug_manifests(history: &[Event], key: &Tuple, grid: &[i64]) -> bool {
    interval_bug_manifests(history, key, grid, IntervalBug::ShortEnd)
}

#[test]
fn mutant_weakening_the_grid_to_stored_coordinates_only_blinds_it_to_a_short_end_boundary_bug() {
    let seeds = 300u64;
    let key: Tuple = Tuple::from_vec(vec![v(0)]);

    let mut caught_without_ticks = 0usize;
    let mut caught_with_ticks = 0usize;
    for seed in 0..seeds {
        // INVARIANT(test_seed_mix): property-test seed diffusion uses modular golden mix.
        let mut rng = Rng::new(0x9BAD_E1D0_u64 ^ mix_seed(seed));
        let params = gen_temporal_params(&mut rng);
        let history = gen_temporal_history(&mut rng, &key, &params);

        let mut stored_only: Vec<i64> = history
            .iter()
            .filter(|e| *e.key() == key)
            .map(|e| e.valid())
            .collect();
        stored_only.sort_unstable();
        stored_only.dedup();
        if short_end_bug_manifests(&history, &key, &stored_only) {
            caught_without_ticks += 1;
        }

        let full_grid = program_grid(&history, Axis::Valid);
        if short_end_bug_manifests(&history, &key, &full_grid) {
            caught_with_ticks += 1;
        }
    }
    assert!(caught_with_ticks > 0);
    assert!(
        caught_without_ticks < caught_with_ticks,
        "±1-tick grid must catch strictly more seeds than coordinates-only \
         (without={caught_without_ticks}, with={caught_with_ticks})"
    );
}

// ════════════════════════════════════════════════════════════════════════
// CAPABILITY 4 — refusal-lift generator: temporal negation, recursion,
// both aggregation families over historical bases.
// ════════════════════════════════════════════════════════════════════════

fn gen_temporal_existential_history(
    rng: &mut Rng,
    key: &Tuple,
    p: &TemporalGenParams,
) -> Vec<Event> {
    let mut history = Vec::new();
    for _ in 0..p.events_per_key {
        let valid = rng.range(-p.coord_span, p.coord_span);
        let sys = rng.range(-p.coord_span, p.coord_span);
        let event = match rng.one_of(&TEMPORAL_POLARITIES) {
            ClaimPolarity::Assert => {
                Event::assert(key.clone(), Tuple::from_vec(vec![]), valid, sys)
            }
            ClaimPolarity::Retract => Event::retract(key.clone(), valid, sys),
            ClaimPolarity::Erase => Event::erase(key.clone(), valid, sys),
        };
        history
            .push(must_ok(event, "coord_span keeps every draw far below the reserved terminal tick"));
        if rng.chance(2, 5) {
            let correction_sys = sys + rng.range(1, 5);
            history.push(
                must_ok(Event::assert(key.clone(), Tuple::from_vec(vec![]), valid, correction_sys), "coord_span keeps every draw far below the reserved terminal tick"),
            );
        }
    }
    history
}

fn neg_lit_at(rel: impl Into<Rel>, args: Vec<Term>, at: AsOf) -> Literal {
    Literal::neg_at(rel, args, at)
}

struct ReachabilityFixture {
    edge_history: Vec<Event>,
    seed_history: Vec<Event>,
    nodes: Vec<i64>,
    c_edge: AsOf,
    c_seed: AsOf,
    meet_op: &'static str,
}

#[cfg(test)]
fn gen_reachability_fixture(rng: &mut Rng) -> ReachabilityFixture {
    let n = rng.range(3, 8);
    let nodes: Vec<i64> = (0..n).collect();
    let params = gen_temporal_params(rng);

    let n_edges = rng.range(1, n * 2);
    let mut edge_history = Vec::new();
    for _ in 0..n_edges {
        let a = rng.range(0, n);
        let b = rng.range(0, n);
        edge_history.extend(gen_temporal_existential_history(
            rng,
            &Tuple::from_vec(vec![v(a), v(b)]),
            &params,
        ));
    }

    let meet_op = rng.one_of(&MEET_OPS);
    let mut seed_history = Vec::new();
    for &node in &nodes {
        if rng.chance(2, 3) {
            let key: Tuple = Tuple::from_vec(vec![v(node)]);
            for _ in 0..rng.range(1, 4) {
                let valid = rng.range(-params.coord_span, params.coord_span);
                let sys = rng.range(-params.coord_span, params.coord_span);
                let payload = match meet_op {
                    "and" | "or" => DataValue::from(rng.chance(1, 2)),
                    other_meet => {
                        assert!(other_meet != "and" && other_meet != "or", "meet arm partition");
                        v(rng.range(-10, 10))
                    }
                };
                seed_history.push(
                    must_ok(Event::assert(key.clone(), Tuple::from_vec(vec![payload]), valid, sys), "coord_span keeps every draw far below the reserved terminal tick"),
                );
            }
        }
    }

    let c_edge = near_coordinate(rng, &edge_history);
    let c_seed = near_coordinate(rng, &seed_history);
    ReachabilityFixture {
        edge_history,
        seed_history,
        nodes,
        c_edge,
        c_seed,
        meet_op,
    }
}

fn reachability_program(rng: &mut Rng, fx: &ReachabilityFixture) -> Program {
    let mut histories: BTreeMap<Rel, Vec<Event>> = BTreeMap::new();
    histories.insert("hedge".into(), fx.edge_history.clone());
    histories.insert("hseed".into(), fx.seed_history.clone());
    let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
    facts.insert(
        "node".into(),
        fx.nodes
            .iter()
            .map(|&n| vec![v(n)])
            .map(Tuple::from_vec)
            .collect(),
    );

    let mut rules = vec![
        Rule::plain(
            "path",
            vec![x(), y()],
            vec![lit_at("hedge", vec![x(), y()], fx.c_edge)],
        ),
        Rule::plain(
            "path",
            vec![x(), z()],
            vec![
                lit("path", vec![x(), y()], false),
                lit_at("hedge", vec![y(), z()], fx.c_edge),
            ],
        ),
        Rule::plain(
            "unreachable",
            vec![x(), y()],
            vec![
                lit("node", vec![x()], false),
                lit("node", vec![y()], false),
                neg_lit_at("hedge", vec![x(), y()], fx.c_edge),
            ],
        ),
        Rule::aggregated(
            "deg",
            vec![x(), y()],
            vec![HeadAggr::Plain, named("count")],
            vec![lit_at("hedge", vec![x(), y()], fx.c_edge)],
        ),
        Rule::aggregated(
            "m",
            vec![x(), y()],
            vec![HeadAggr::Plain, named(fx.meet_op)],
            vec![lit_at("hseed", vec![x(), y()], fx.c_seed)],
        ),
        Rule::aggregated(
            "m",
            vec![y(), z()],
            vec![HeadAggr::Plain, named(fx.meet_op)],
            vec![
                lit_at("hedge", vec![x(), y()], fx.c_edge),
                lit("m", vec![x(), z()], false),
            ],
        ),
    ];
    for rule in &mut rules {
        shuffle_body(rng, &mut rule.body);
    }
    Program {
        rules,
        facts,
        histories,
        ..Program::empty()
    }
}

fn brute_force_closure(edges: &BTreeSet<Tuple>) -> BTreeSet<Tuple> {
    let mut closure = edges.clone();
    loop {
        let mut additions = Vec::new();
        for e1 in &closure {
            for e2 in &closure {
                if e1[1] == e2[0] {
                    let candidate: Tuple = Tuple::from_vec(vec![e1[0].clone(), e2[1].clone()]);
                    if !closure.contains(&candidate) {
                        additions.push(candidate);
                    }
                }
            }
        }
        if additions.is_empty() {
            break;
        }
        closure.extend(additions);
    }
    closure
}

fn expected_unreachable(nodes: &[i64], edges: &BTreeSet<Tuple>) -> BTreeSet<Tuple> {
    let mut out = BTreeSet::new();
    for &a in nodes {
        for &b in nodes {
            let t: Tuple = Tuple::from_vec(vec![v(a), v(b)]);
            if !edges.contains(&t) {
                out.insert(t);
            }
        }
    }
    out
}

fn expected_degree(edges: &BTreeSet<Tuple>) -> BTreeSet<Tuple> {
    let mut counts: BTreeMap<DataValue, i64> = BTreeMap::new();
    for e in edges {
        *counts.entry(e[0].clone()).or_insert(0) += 1;
    }
    counts
        .into_iter()
        .map(|(k, c)| vec![k, v(c)])
        .map(Tuple::from_vec)
        .collect()
}

#[cfg(test)]
/// Meet propagation via the oracle [`AggrFold`] seam (fold reuses real
/// builtins; the LOOP stays independent of the Datalog evaluator).
fn expected_meet(
    edges: &BTreeSet<Tuple>,
    seeds: &BTreeSet<Tuple>,
    meet_op: &str,
) -> BTreeSet<Tuple> {
    let fold: Arc<dyn AggrFold> = must_ok(builtin_fold(meet_op), "meet fold exists");
    let mut op = must_ok(fold.fresh_meet(), "meet-capable");
    let mut acc: BTreeMap<DataValue, MeetAccum> = BTreeMap::new();
    for row in seeds {
        acc.insert(row[0].clone(), MeetAccum::from_derived(row[1].clone()));
    }
    let mut steps = 0usize;
    loop {
        steps += 1;
        assert!(
            steps <= 4 * edges.len() + 4,
            "meet propagation failed to terminate"
        );
        let mut changed = false;
        for edge in edges {
            let (a, b) = (&edge[0], &edge[1]);
            let Some(val) = acc.get(a).cloned() else {
                continue;
            };
            match acc.get(b).cloned() {
                None => {
                    acc.insert(b.clone(), val);
                    changed = true;
                }
                Some(mut cur) => {
                    if must_ok(op.update(&mut cur, &val), "meet update") {
                        acc.insert(b.clone(), cur);
                        changed = true;
                    }
                }
            }
        }
        if !changed {
            break;
        }
    }
    acc.into_iter()
        .map(|(k, val)| vec![k, val.to_value()])
        .map(Tuple::from_vec)
        .collect()
}

#[test]
fn temporal_negation_recursion_and_both_aggregation_families_match_independent_references() {
    let mut cases = 0usize;
    let seeds = 400u64;
    for seed in 0..seeds {
        // INVARIANT(test_seed_mix): property-test seed diffusion uses modular golden mix.
        let mut rng = Rng::new(0xF00D_BA11_u64 ^ mix_seed(seed));
        let fx = gen_reachability_fixture(&mut rng);
        let program = reachability_program(&mut rng, &fx);
        let db = must_ok(
            naive_eval(&program),
            "negation over a fixed as-of historical relation is legal (the lift); \
             recursion and both aggregation families over historical leaves are well-formed",
        );

        let edge_snapshot = resolve_relation(&fx.edge_history, fx.c_edge);
        let seed_snapshot = resolve_relation(&fx.seed_history, fx.c_seed);

        assert_eq!(
            match db.get("path") { Some(s) => s.clone(), None => BTreeSet::new() },
            brute_force_closure(&edge_snapshot),
            "seed {seed}: path"
        );
        cases += 1;

        assert_eq!(
            match db.get("unreachable") { Some(s) => s.clone(), None => BTreeSet::new() },
            expected_unreachable(&fx.nodes, &edge_snapshot),
            "seed {seed}: unreachable"
        );
        cases += 1;

        assert_eq!(
            match db.get("deg") { Some(s) => s.clone(), None => BTreeSet::new() },
            expected_degree(&edge_snapshot),
            "seed {seed}: deg"
        );
        cases += 1;

        assert_eq!(
            match db.get("m") { Some(s) => s.clone(), None => BTreeSet::new() },
            expected_meet(&edge_snapshot, &seed_snapshot, fx.meet_op),
            "seed {seed}: m (op={})",
            fx.meet_op
        );
        cases += 1;
    }
    assert!(
        cases >= 800,
        "expected hundreds of temporal negation/recursion/aggregation cases, ran {cases}"
    );
}
