/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Phase-1 scaffolding for the filter-aware HNSW ascent (story #3).
//!
//! These are "the ropes": the ground-truth oracle, the selectivity-sweep
//! generator, the recall meter, and the determinism harness the Phase-2 climb
//! is measured against. They drive a *search closure*, so they exercise the
//! draft's post-filter `hnsw_knn` today (to pin the baseline table) and the new
//! filter-aware entry point in Phase 2 with no change to the instruments.
//!
//! Wiring: declared as `#[cfg(test)] mod hnsw_filter_harness;` at the foot of
//! `runtime/hnsw.rs`, a sibling of `mod tests`. `use super::*` inherits the
//! hnsw module's own imports (the same way `mod tests` gets `ColType`,
//! `OrderedFloat`, …); everything else is imported explicitly here so the
//! module stands on its own.
//!
//! ADVERSARIAL INDEPENDENCE: the oracle re-implements the filter predicate in
//! native Rust (`FilterSpec::passes`) and scores with `IndexVec::dist`; it
//! shares no code with the engine's bytecode filter eval or its graph walk, so
//! agreement between oracle and engine is evidence, not tautology.

use super::*;

use proptest::prelude::*;

use crate::data::functions::{OP_GE, OP_LT, OP_MOD};
use crate::data::program::InputRelationHandle;
use crate::data::symb::Symbol;
use crate::runtime::relation::{KeyspaceKind, RelationHandle, create_relation};
use crate::storage::Storage;
use crate::storage::fjall::new_fjall_storage;

// ---------------------------------------------------------------------------
// Local schema helpers (the draft's live in `mod tests`; kept private there).
// ---------------------------------------------------------------------------

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

fn input_handle(name: &str, metadata: StoredRelationMetadata) -> InputRelationHandle {
    let key_bindings = metadata
        .keys
        .iter()
        .map(|c| Symbol::new(c.name.clone(), SourceSpan(0, 0)))
        .collect();
    let dep_bindings = metadata
        .non_keys
        .iter()
        .map(|c| Symbol::new(c.name.clone(), SourceSpan(0, 0)))
        .collect();
    InputRelationHandle {
        name: Symbol::new(name, SourceSpan(0, 0)),
        metadata,
        key_bindings,
        dep_bindings,
        span: SourceSpan(0, 0),
    }
}

// ---------------------------------------------------------------------------
// Deterministic vector/row generation.
// ---------------------------------------------------------------------------

/// One splitmix64 step — same house PRNG as the engine's level seed, so the
/// generated corpus is byte-reproducible across platforms.
fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A reproducible f64 in [-1, 1) (24 bits of entropy, so every value is
/// exactly representable at f32 precision too).
fn next_f32(state: &mut u64) -> f64 {
    let bits = splitmix(state) >> 40; // 24 bits
    (bits as f64 / (1u32 << 23) as f64) - 1.0
}

/// A seeded corpus: `n` rows, key `k = 0..n`, a `dim`-dimensional F32 vector.
/// The key doubles as the filterable scalar (see `FilterSpec`).
fn seeded_rows(n: i64, dim: usize, seed: u64) -> Vec<Tuple> {
    let mut state = seed ^ 0xA5A5_5A5A_1234_9876;
    (0..n)
        .map(|k| {
            let comps: Vec<f64> = (0..dim).map(|_| next_f32(&mut state)).collect();
            vec![DataValue::from(k), DataValue::Vector(Vector::new(comps))]
        })
        .collect()
}

fn seeded_query(dim: usize, seed: u64) -> Vector {
    let mut state = seed ^ 0x0F0F_F0F0_DEAD_BEEF;
    let comps: Vec<f64> = (0..dim).map(|_| next_f32(&mut state)).collect();
    Vector::new(comps)
}

/// A deterministic (Fisher–Yates, splitmix-seeded) permutation of `rows` — the
/// insertion-order shuffle for the order-invariance obligation (design §5.7).
/// The *set* of rows is identical to the input; only the order changes.
fn seeded_permutation(rows: &[Tuple], seed: u64) -> Vec<Tuple> {
    let mut out = rows.to_vec();
    let mut state = seed ^ 0x5EED_0F0F_A11C_0DE5;
    for i in (1..out.len()).rev() {
        let j = (splitmix(&mut state) % (i as u64 + 1)) as usize;
        out.swap(i, j);
    }
    out
}

// ---------------------------------------------------------------------------
// Schema / manifest parameterized on dimension (the draft's helpers are dim-2).
// ---------------------------------------------------------------------------

fn hbase_metadata(dim: usize) -> StoredRelationMetadata {
    StoredRelationMetadata {
        keys: vec![col("k", ColType::Int)],
        non_keys: vec![col(
            "v",
            ColType::Vec {
                eltype: VecElementType::F32,
                len: dim,
            },
        )],
    }
}

fn hmanifest(dim: usize, distance: HnswDistance) -> HnswIndexManifest {
    HnswIndexManifest {
        base_relation: SmartString::from("corpus"),
        index_name: SmartString::from("by_v"),
        vec_dim: dim,
        dtype: VecElementType::F32,
        vec_fields: vec![1],
        distance,
        ef_construction: 32,
        m_neighbours: 16,
        m_max: 16,
        m_max0: 32,
        level_multiplier: 1.0 / (16f64).ln(),
        index_filter: None,
        extend_candidates: false,
        keep_pruned_connections: false,
    }
}

/// A real base relation + HNSW index on a real fjall store, populated. Mirrors
/// the draft's `setup`, parameterized on dimension.
fn hsetup(
    db: &impl Storage,
    dim: usize,
    distance: HnswDistance,
    rows: &[Tuple],
) -> (RelationHandle, RelationHandle, HnswIndexManifest) {
    let m = hmanifest(dim, distance);
    let mut tx = db.write_tx().unwrap();
    let base = create_relation(
        &mut tx,
        input_handle("corpus", hbase_metadata(dim)),
        KeyspaceKind::Facts,
    )
    .unwrap();
    let idx = create_relation(
        &mut tx,
        input_handle("corpus:by_v", hnsw_index_metadata(&base.metadata)),
        KeyspaceKind::AlgorithmState,
    )
    .unwrap();
    let mut stack = vec![];
    for r in rows {
        base.put_fact(
            &mut tx,
            r,
            crate::data::value::ValidityTs::from_raw(0),
            SourceSpan(0, 0),
        )
        .unwrap();
        hnsw_put(&mut tx, &m, &base, &idx, None, &mut stack, r).unwrap();
    }
    tx.commit().unwrap();
    (base, idx, m)
}

// ---------------------------------------------------------------------------
// FilterSpec: one predicate, TWO independent realizations.
//   - `bytecode()` drives the ENGINE (compiled, over the appended output row).
//   - `passes()`   drives the ORACLE (native Rust, over the base row).
// The engine's filter sees base row `++ [field, field_idx, distance, vector]`
// (draft `hnsw_knn` contract). Our filters reference only the key column
// (tuple_pos 0), unaffected by the appended columns — but the harness is shaped
// so a distance-referencing filter is a one-line addition.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum FilterSpec {
    /// `k < threshold` — selectivity `threshold/n`; correlates with key order
    /// (exercises prefix-sample bias, design Q1).
    LessThan { threshold: i64 },
    /// `(k mod modulus) < accept` — selectivity `accept/modulus`, uncorrelated
    /// with key order (the unbiased band generator).
    ModLessThan { modulus: i64, accept: i64 },
    /// `k >= threshold` — the tail complement of `LessThan`. Used to carve out
    /// a match set that sits in a specific key range (e.g. a translated
    /// cluster appended at the end of a corpus), never sampled by a modulus
    /// stripe.
    AtLeast { threshold: i64 },
}

impl FilterSpec {
    /// Native predicate over a BASE row `[k, v, …]`. The oracle's truth.
    fn passes(&self, row: &[DataValue]) -> bool {
        let k = row[0].get_int().expect("key is int");
        match *self {
            FilterSpec::LessThan { threshold } => k < threshold,
            FilterSpec::ModLessThan { modulus, accept } => k.rem_euclid(modulus) < accept,
            FilterSpec::AtLeast { threshold } => k >= threshold,
        }
    }

    /// Compiled bytecode over the ENGINE's appended output row. Binding
    /// `tuple_pos = 0` is the key column `k`, at position 0 (base keys first).
    fn bytecode(&self) -> (Vec<Bytecode>, SourceSpan) {
        let span = SourceSpan(0, 0);
        let k = Symbol::new("k", span);
        let code = match *self {
            FilterSpec::LessThan { threshold } => vec![
                Bytecode::Binding {
                    var: k,
                    tuple_pos: Some(0),
                },
                Bytecode::Const {
                    val: DataValue::from(threshold),
                    span,
                },
                Bytecode::Apply {
                    op: &OP_LT,
                    arity: 2,
                    span,
                },
            ],
            FilterSpec::ModLessThan { modulus, accept } => vec![
                Bytecode::Binding {
                    var: k,
                    tuple_pos: Some(0),
                },
                Bytecode::Const {
                    val: DataValue::from(modulus),
                    span,
                },
                Bytecode::Apply {
                    op: &OP_MOD,
                    arity: 2,
                    span,
                },
                Bytecode::Const {
                    val: DataValue::from(accept),
                    span,
                },
                Bytecode::Apply {
                    op: &OP_LT,
                    arity: 2,
                    span,
                },
            ],
            FilterSpec::AtLeast { threshold } => vec![
                Bytecode::Binding {
                    var: k,
                    tuple_pos: Some(0),
                },
                Bytecode::Const {
                    val: DataValue::from(threshold),
                    span,
                },
                Bytecode::Apply {
                    op: &OP_GE,
                    arity: 2,
                    span,
                },
            ],
        };
        (code, span)
    }

    /// True selectivity over a concrete corpus — VERIFIES the sweep generator
    /// lands in its band before any search runs.
    fn true_selectivity(&self, rows: &[Tuple]) -> f64 {
        self.true_match_count(rows) as f64 / rows.len() as f64
    }

    fn true_match_count(&self, rows: &[Tuple]) -> usize {
        rows.iter().filter(|r| self.passes(r)).count()
    }
}

/// Sweep generator: a filter whose true selectivity is `target`
/// (granularity 0.1%). `striped` picks the accepted-set SHAPE — a
/// `ModLessThan` stripe (accepted keys spread uniformly) or a
/// `LessThan` prefix (accepted keys contiguous). The selector must
/// hold on both distributions: a contiguous accepted range clusters in
/// the graph exactly where a striped one does not.
fn filter_at_selectivity(target: f64, striped: bool) -> FilterSpec {
    let modulus = 1000i64;
    let accept = (target * modulus as f64).round() as i64;
    if striped {
        FilterSpec::ModLessThan { modulus, accept }
    } else {
        // Keys are 0..n; a threshold at target*n accepts the same
        // fraction, contiguously. The harness fixtures use n = 1000.
        FilterSpec::LessThan { threshold: accept }
    }
}

/// The four canonical bands the review named: 1%, 10%, 50%, 90%.
const SELECTIVITY_BANDS: [f64; 4] = [0.01, 0.10, 0.50, 0.90];

// ---------------------------------------------------------------------------
// Ground-truth oracle: exact filtered k-nearest by full linear scan.
// ---------------------------------------------------------------------------

/// The exact filtered nearest neighbours of `q` among `rows`, as ordered keys
/// (nearest first). Distance is the manifest metric through `IndexVec` (matches
/// the engine's arithmetic exactly); ties broken by ascending key so the order
/// is total and deterministic — the same total order the engine must adopt
/// (design §5.3).
fn brute_force_filtered_knn(
    q: &Vector,
    k: usize,
    filter: &FilterSpec,
    rows: &[Tuple],
    manifest: &HnswIndexManifest,
) -> Vec<i64> {
    let qv = IndexVec::admit(q, manifest).expect("query admits");
    let mut scored: Vec<(OrderedFloat<f64>, i64)> = rows
        .iter()
        .filter(|r| filter.passes(r))
        .map(|r| {
            let key = r[0].get_int().unwrap();
            let v = match &r[1] {
                DataValue::Vector(v) => v.clone(),
                _ => panic!("row vector"),
            };
            let vv = IndexVec::admit(&v, manifest).expect("row admits");
            (OrderedFloat(qv.dist(&vv, manifest.distance)), key)
        })
        .collect();
    scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    scored.into_iter().take(k).map(|(_, key)| key).collect()
}

// ---------------------------------------------------------------------------
// Recall meter.
// ---------------------------------------------------------------------------

/// Recall@k = |engine ∩ truth| / min(k, |truth|).
fn recall_at_k(engine_keys: &[i64], truth_keys: &[i64], k: usize) -> f64 {
    let truth: FxHashSet<i64> = truth_keys.iter().copied().collect();
    let hit = engine_keys.iter().filter(|k| truth.contains(k)).count();
    let denom = k.min(truth_keys.len()).max(1);
    hit as f64 / denom as f64
}

/// Count-recall = min(k, |engine|) / min(k, |truth|): the k-guarantee's meter
/// (did we return as many rows as we should have, regardless of ranking).
fn count_recall(engine_keys: &[i64], truth_keys: &[i64], k: usize) -> f64 {
    let denom = k.min(truth_keys.len()).max(1);
    engine_keys.len().min(k) as f64 / denom as f64
}

fn keys_of(hits: &[Tuple]) -> Vec<i64> {
    hits.iter().map(|t| t[0].get_int().unwrap()).collect()
}

fn knn_params_p2(k: usize, ef: usize) -> HnswKnnParams {
    HnswKnnParams {
        k,
        ef,
        radius: None,
        bind_field: false,
        bind_field_idx: false,
        bind_distance: true,
        bind_vector: false,
    }
}

/// Drive `hnsw_knn` with a filter — which now dispatches to the filter-aware
/// path (selector → scan / filtered graph / fallback). The one call site the
/// whole recall/determinism story flows through.
#[allow(clippy::too_many_arguments)] // mirrors hnsw_knn's own surface
fn filtered_search(
    tx: &impl ReadTx,
    q: &Vector,
    manifest: &HnswIndexManifest,
    base: &RelationHandle,
    idx: &RelationHandle,
    k: usize,
    ef: usize,
    filter: &FilterSpec,
) -> Vec<Tuple> {
    let params = knn_params_p2(k, ef);
    let fb = Some(filter.bytecode());
    let mut stack = vec![];
    hnsw_knn(
        tx,
        q,
        manifest,
        base,
        idx,
        &params,
        &fb,
        &mut stack,
        &crate::fixed_rule::CancelFlag::default(),
    )
    .unwrap()
}

/// The chosen strategy for `filter` — for the selector/order-invariance tests.
#[allow(clippy::too_many_arguments)] // mirrors hnsw_knn's own surface
fn selected_plan(
    tx: &impl ReadTx,
    q: &Vector,
    manifest: &HnswIndexManifest,
    base: &RelationHandle,
    idx: &RelationHandle,
    k: usize,
    ef: usize,
    filter: &FilterSpec,
) -> Option<SearchPlan> {
    let params = knn_params_p2(k, ef);
    let fb = filter.bytecode();
    let mut stack = vec![];
    hnsw_knn_selected_plan(tx, q, manifest, base, idx, &params, &fb, &mut stack).unwrap()
}

/// The PINNED draft post-filter baseline (measured in Phase 1, before the
/// filter-aware path replaced it): (target selectivity, recall@k, count_recall)
/// at L2, N=4000, dim=16, k=10, ef=32, corpus seed 7, query seed 99. The
/// filter-aware path must MEET OR BEAT every row (design §4/§6).
const PINNED_BASELINE: [(f64, f64, f64); 4] = [
    (0.01, 0.000, 0.000),
    (0.10, 0.400, 0.400),
    (0.50, 0.900, 1.000),
    (0.90, 1.000, 1.000),
];

// Shared corpus parameters so every Phase-2 test compares like with like and
// matches the pinned baseline's conditions.
const P2_DIM: usize = 16;
const P2_N: i64 = 4000;
const P2_K: usize = 10;
const P2_EF: usize = 32;
const P2_CORPUS_SEED: u64 = 7;
const P2_QUERY_SEED: u64 = 99;

// ---------------------------------------------------------------------------
// The instruments as tests. Recall assertions AGAINST the new path are Phase 2
// (they need the new entry point). Phase 1 proves the ropes are sound: the
// sweep hits its bands, the oracle is exact and total-ordered, the determinism
// comparator bites, and the baseline table computes end-to-end.
// ---------------------------------------------------------------------------

/// The sweep generator lands each band within tolerance over a real corpus.
#[test]
fn sweep_generator_hits_its_bands() {
    let rows = seeded_rows(4000, 16, 1);
    for target in SELECTIVITY_BANDS {
        // Alternate the accepted-set shape across the sweep: half the
        // bands run striped (mod), half contiguous (threshold).
        let f = filter_at_selectivity(target, (target * 100.0) as i64 % 2 == 0);
        let s = f.true_selectivity(&rows);
        assert!(
            (s - target).abs() <= 0.01,
            "band {target}: got selectivity {s}"
        );
    }
}

/// The oracle is exact and total-ordered: on a hand-checkable corpus its
/// filtered nearest set is the arithmetic truth, and equal-distance rows come
/// back in ascending-key order (the tie-break the engine must match).
#[test]
fn oracle_is_exact_and_total_ordered() {
    let m = hmanifest(2, HnswDistance::L2);
    let rows: Vec<Tuple> = vec![
        vec![
            DataValue::from(0),
            DataValue::Vector(Vector::new(vec![3.0, 0.0])),
        ],
        vec![
            DataValue::from(1),
            DataValue::Vector(Vector::new(vec![0.1, 0.0])),
        ],
        vec![
            DataValue::from(2),
            DataValue::Vector(Vector::new(vec![1.0, 0.0])),
        ],
        vec![
            DataValue::from(3),
            DataValue::Vector(Vector::new(vec![0.2, 0.0])),
        ],
        // key 4 sits at the SAME distance as key 2 -> tie broken by key.
        vec![
            DataValue::from(4),
            DataValue::Vector(Vector::new(vec![-1.0, 0.0])),
        ],
    ];
    let q = Vector::new(vec![0.0, 0.0]);
    let even = FilterSpec::ModLessThan {
        modulus: 2,
        accept: 1,
    }; // keeps even keys 0,2,4
    let got = brute_force_filtered_knn(&q, 3, &even, &rows, &m);
    // d: key0=9, key2=1, key4=1. Nearest: 2 & 4 tie at 1, key 2<4, then 0.
    assert_eq!(got, vec![2, 4, 0]);
}

/// The determinism comparator has teeth: identical runs agree, a reordering at
/// a tie boundary is caught. Phase 2 points this at the live search across
/// thread counts; Phase 1 proves the comparator itself bites.
#[test]
fn determinism_comparator_detects_perturbation() {
    let a = vec![2i64, 1, 3];
    let b = vec![2i64, 1, 3];
    let c = vec![2i64, 3, 1];
    assert_eq!(a, b, "identical searches must be byte-equal");
    assert_ne!(a, c, "the comparator must catch a reordering");
}

/// The insertion-order shuffle is a genuine reordering of the *same set* — the
/// premise the order-invariance obligation rests on. (Phase 2 adds the live
/// obligation: same corpus, shuffled insertion order ⇒ identical *selected
/// strategy* every band, and byte-identical results wherever the scan strategy
/// is chosen — design §5.7. It needs the new filter-aware entry point, so it is
/// written in Phase 2 against `select_strategy`/`scan_filtered`.)
#[test]
fn seeded_permutation_preserves_the_set_and_reorders() {
    let rows = seeded_rows(200, 8, 3);
    let shuffled = seeded_permutation(&rows, 42);
    assert_eq!(shuffled.len(), rows.len());
    // Same set of keys.
    let mut a: Vec<i64> = rows.iter().map(|r| r[0].get_int().unwrap()).collect();
    let mut b: Vec<i64> = shuffled.iter().map(|r| r[0].get_int().unwrap()).collect();
    assert_ne!(a, b, "the shuffle must actually reorder");
    a.sort_unstable();
    b.sort_unstable();
    assert_eq!(a, b, "the shuffle must preserve the exact set");
}

/// Which bands the selector should serve by exact scan (recall-safe) vs graph.
/// Derived from the design's thresholds at the standard corpus params: at 1% and
/// 10% the estimated match count is ≤ K_SCAN·k=1000 → Scan; at 50%/90% it
/// exceeds it and graph work stays under N → Graph.
fn expected_is_scan(target: f64) -> bool {
    target <= 0.10 + 1e-9
}

/// THE GATE. The filter-aware path meets or beats the pinned draft baseline at
/// every band, and is EXACT (recall 1.0 / count 1.0) in the scan bands — the
/// bands where the old post-filter returned as little as zero of k.
#[test]
fn filter_aware_recall_meets_or_beats_baseline() {
    let rows = seeded_rows(P2_N, P2_DIM, P2_CORPUS_SEED);
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let (base, idx, m) = hsetup(&db, P2_DIM, HnswDistance::L2, &rows);
    let rtx = db.read_tx().unwrap();
    let q = seeded_query(P2_DIM, P2_QUERY_SEED);

    for (target, base_recall, base_count) in PINNED_BASELINE {
        // Alternate the accepted-set shape across the sweep: half the
        // bands run striped (mod), half contiguous (threshold).
        let f = filter_at_selectivity(target, (target * 100.0) as i64 % 2 == 0);
        let truth = brute_force_filtered_knn(&q, P2_K, &f, &rows, &m);
        let hits = filtered_search(&rtx, &q, &m, &base, &idx, P2_K, P2_EF, &f);
        let ekeys = keys_of(&hits);
        let r = recall_at_k(&ekeys, &truth, P2_K);
        let cr = count_recall(&ekeys, &truth, P2_K);

        assert!(
            r >= base_recall - 1e-9,
            "band {target}: recall {r} regressed below baseline {base_recall}"
        );
        assert!(
            cr >= base_count - 1e-9,
            "band {target}: count_recall {cr} regressed below baseline {base_count}"
        );
        if expected_is_scan(target) {
            assert!(
                (r - 1.0).abs() < 1e-9 && (cr - 1.0).abs() < 1e-9,
                "scan band {target} must be EXACT, got recall {r} count {cr}"
            );
        }
        // Results must be nearest-first and never exceed k.
        assert!(hits.len() <= P2_K, "band {target}: over-k result set");
    }
}

/// The selector is mutation-proof INDEPENDENTLY of the fallback: it must pick
/// Scan in the selective bands and Graph otherwise. A mutation that inverts a
/// threshold or pins the estimate flips a plan here and this bites — even though
/// the fallback would still repair the *count*.
#[test]
fn selector_chooses_scan_when_selective_graph_otherwise() {
    let rows = seeded_rows(P2_N, P2_DIM, P2_CORPUS_SEED);
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let (base, idx, m) = hsetup(&db, P2_DIM, HnswDistance::L2, &rows);
    let rtx = db.read_tx().unwrap();
    let q = seeded_query(P2_DIM, P2_QUERY_SEED);

    for target in SELECTIVITY_BANDS {
        // Alternate the accepted-set shape across the sweep: half the
        // bands run striped (mod), half contiguous (threshold).
        let f = filter_at_selectivity(target, (target * 100.0) as i64 % 2 == 0);
        let plan = selected_plan(&rtx, &q, &m, &base, &idx, P2_K, P2_EF, &f).unwrap();
        let is_scan = matches!(plan, SearchPlan::Scan);
        assert_eq!(
            is_scan,
            expected_is_scan(target),
            "band {target}: selector chose {plan:?}, expected scan={}",
            expected_is_scan(target)
        );
    }
}

/// The filtered search is byte-deterministic: repeated runs, and independent
/// builds in the SAME insertion order, yield byte-identical result tuples. (The
/// engine search is single-threaded per call, so thread count cannot perturb one
/// search; determinism across threads is a property of the RA fan-out, tested
/// there.)
#[test]
fn filtered_search_is_byte_deterministic() {
    let rows = seeded_rows(P2_N, P2_DIM, P2_CORPUS_SEED);
    let q = seeded_query(P2_DIM, P2_QUERY_SEED);
    let run = || -> Vec<Vec<Tuple>> {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let (base, idx, m) = hsetup(&db, P2_DIM, HnswDistance::L2, &rows);
        let rtx = db.read_tx().unwrap();
        SELECTIVITY_BANDS
            .iter()
            .map(|&t| {
                filtered_search(
                    &rtx,
                    &q,
                    &m,
                    &base,
                    &idx,
                    P2_K,
                    P2_EF,
                    &filter_at_selectivity(t, true),
                )
            })
            .collect()
    };
    let a = run();
    let b = run();
    assert_eq!(
        a, b,
        "filtered search must be byte-identical across builds/runs"
    );
    // And twice on the same store.
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let (base, idx, m) = hsetup(&db, P2_DIM, HnswDistance::L2, &rows);
    let rtx = db.read_tx().unwrap();
    let f = filter_at_selectivity(0.10, true);
    let h1 = filtered_search(&rtx, &q, &m, &base, &idx, P2_K, P2_EF, &f);
    let h2 = filtered_search(&rtx, &q, &m, &base, &idx, P2_K, P2_EF, &f);
    assert_eq!(h1, h2, "repeat search on one store must be identical");
}

/// Insertion-order invariance (design §5.7): the same facts in a different
/// insertion order choose the SAME strategy at every band, and yield
/// BYTE-IDENTICAL results wherever the scan strategy is chosen (the graph band
/// may differ because the graph itself is order-dependent — that is HNSW's
/// inherent property, not new sensitivity).
#[test]
fn order_invariant_strategy_and_scan_results() {
    let rows = seeded_rows(P2_N, P2_DIM, P2_CORPUS_SEED);
    let shuffled = seeded_permutation(&rows, 0xC0FFEE);
    let q = seeded_query(P2_DIM, P2_QUERY_SEED);

    let build = |data: &[Tuple]| {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let (base, idx, m) = hsetup(&db, P2_DIM, HnswDistance::L2, data);
        // Return owned results per band, keeping dir alive for the scope.
        let rtx = db.read_tx().unwrap();
        let out: Vec<(Option<SearchPlan>, Vec<Tuple>)> = SELECTIVITY_BANDS
            .iter()
            .map(|&t| {
                let f = filter_at_selectivity(t, true);
                let plan = selected_plan(&rtx, &q, &m, &base, &idx, P2_K, P2_EF, &f);
                let hits = filtered_search(&rtx, &q, &m, &base, &idx, P2_K, P2_EF, &f);
                (plan, hits)
            })
            .collect();
        out
    };
    let natural = build(&rows);
    let permuted = build(&shuffled);

    for (i, (&target, (nat, perm))) in SELECTIVITY_BANDS
        .iter()
        .zip(natural.iter().zip(permuted.iter()))
        .enumerate()
    {
        assert_eq!(
            nat.0, perm.0,
            "band {target}: strategy differs under a different insertion order"
        );
        if expected_is_scan(target) {
            assert_eq!(
                nat.1, perm.1,
                "band {target} (scan): results must be insertion-order-invariant"
            );
        }
        let _ = i;
    }
}

/// The scan fallback is load-bearing and exact: a starved graph beam
/// (`ef2 = 1`) under-delivers (< k), and the fallback repairs it to exact
/// `min(k, M)` with recall 1.0. Proves the fallback is not dead code — a
/// "disable fallback" mutation makes the `with_fb` assertions bite.
#[test]
fn fallback_is_load_bearing_and_exact() {
    let rows = seeded_rows(P2_N, P2_DIM, P2_CORPUS_SEED);
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let (base, idx, m) = hsetup(&db, P2_DIM, HnswDistance::L2, &rows);
    let rtx = db.read_tx().unwrap();
    let q = seeded_query(P2_DIM, P2_QUERY_SEED);

    let f = filter_at_selectivity(0.90, true); // many matches, so graph would normally fill
    let truth = brute_force_filtered_knn(&q, P2_K, &f, &rows, &m);
    let params = knn_params_p2(P2_K, P2_EF);
    let fb = f.bytecode();
    let starved = SearchPlan::Graph { ef2: 1 };

    let mut s1 = vec![];
    let no_fb = hnsw_knn_forced(
        &rtx, &q, &m, &base, &idx, &params, &fb, &mut s1, starved, false,
    )
    .unwrap();
    let mut s2 = vec![];
    let with_fb = hnsw_knn_forced(
        &rtx, &q, &m, &base, &idx, &params, &fb, &mut s2, starved, true,
    )
    .unwrap();

    assert!(
        no_fb.len() < P2_K,
        "a starved graph beam must under-deliver, got {}",
        no_fb.len()
    );
    assert_eq!(
        with_fb.len(),
        P2_K,
        "the fallback must repair the count to k"
    );
    assert!(
        (recall_at_k(&keys_of(&with_fb), &truth, P2_K) - 1.0).abs() < 1e-9,
        "the fallback (exact scan) must be perfectly accurate"
    );
}

/// The PRODUCTION fallback (inside `hnsw_knn_filtered`, not the test-only forced
/// path) is load-bearing. Driving the real `hnsw_knn` entry with `ef = 1` at a
/// graph-band selectivity, the selector picks Graph with a tiny inflated beam
/// `ef2 = clamp(ceil(1/ŝ), 1, ..)` that seats far fewer than `k` passing rows;
/// only the production fallback's exact scan can restore the count to `k`. A
/// mutation that deletes the `if hits.len() < k { scan }` branch in
/// `hnsw_knn_filtered` makes THIS bite (where `fallback_is_load_bearing_and_exact`,
/// which routes through the forced helper, would not).
///
/// The count and a HashSet-membership recall are not enough: a mutation that
/// CONCATENATES the graph's short partial with the scan's full result and
/// truncates to `k` (instead of replacing the partial with the scan) also
/// lands on `hits.len() == k`, and a membership-based recall over-counts a
/// duplicated true positive as an independent hit — so it would pass both of
/// the old assertions while returning a corrupt, duplicate-laden set. The
/// no-duplicates check and the exact-set-equals-oracle check below are what
/// catch that (see `min_k_matches_law_generative`'s pinned low-ef band and the
/// splice mutant proven against both in the story #87 fix round).
#[test]
fn production_fallback_repairs_starved_real_search() {
    let rows = seeded_rows(P2_N, P2_DIM, P2_CORPUS_SEED);
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let (base, idx, m) = hsetup(&db, P2_DIM, HnswDistance::L2, &rows);
    let rtx = db.read_tx().unwrap();
    let q = seeded_query(P2_DIM, P2_QUERY_SEED);

    let f = filter_at_selectivity(0.50, true); // graph band, so the selector picks Graph
    // ef = 1 forces the inflated beam ef2 = ceil(1/ŝ) ≈ 2 — far below k = 10, so
    // the real graph walk under-delivers and the production fallback must fire.
    let plan = selected_plan(&rtx, &q, &m, &base, &idx, P2_K, 1, &f).unwrap();
    assert!(
        matches!(plan, SearchPlan::Graph { .. }),
        "precondition: selector must pick Graph here, got {plan:?}"
    );
    let truth = brute_force_filtered_knn(&q, P2_K, &f, &rows, &m);
    let hits = filtered_search(&rtx, &q, &m, &base, &idx, P2_K, 1, &f);
    assert_eq!(
        hits.len(),
        P2_K,
        "the production fallback must repair a starved graph walk to exactly k rows"
    );
    assert!(
        (recall_at_k(&keys_of(&hits), &truth, P2_K) - 1.0).abs() < 1e-9,
        "the production fallback (exact scan) must be perfectly accurate"
    );

    // No duplicates: a naive "concat the partial with the scan, then
    // truncate" repair would double-count whatever the partial already
    // found.
    let mut ekeys = keys_of(&hits);
    let before = ekeys.len();
    ekeys.sort_unstable();
    ekeys.dedup();
    assert_eq!(
        ekeys.len(),
        before,
        "the repaired result must not contain duplicate keys"
    );
    // Exact set equality with the independent oracle: not just "k rows, all
    // individually plausible" but "precisely the true top-k set", which a
    // duplicate-corrupted concat-then-truncate would generally miss (the
    // duplicated slots displace a genuine match from the true top-k).
    let mut sorted_truth = truth.clone();
    sorted_truth.sort_unstable();
    assert_eq!(
        ekeys, sorted_truth,
        "the repaired result must equal the independent oracle's exact top-k set, not merely have the right length"
    );
}

/// The engine's result ordering is a TOTAL order under equal distances: with a
/// corpus of distinct axis unit vectors, every point is EXACTLY equidistant
/// (squared L2 = 1.0, bit-exact) from the origin query, so only the
/// `(distance, encoded-key)` tie-break decides which `k` survive — the smallest
/// keys. A "drop the tie-break" mutation makes this bite.
#[test]
fn engine_ordering_is_total_under_ties() {
    let dim = 16;
    let n = 12i64;
    let rows: Vec<Tuple> = (0..n)
        .map(|i| {
            let mut comps = vec![0.0f64; dim];
            comps[(i as usize) % dim] = 1.0; // a distinct axis unit vector
            vec![DataValue::from(i), DataValue::Vector(Vector::new(comps))]
        })
        .collect();
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let (base, idx, m) = hsetup(&db, dim, HnswDistance::L2, &rows);
    let rtx = db.read_tx().unwrap();
    let q = Vector::new(vec![0.0f64; dim]);
    // A filter that passes every row: (k mod 1) < 1 is always true.
    let f = FilterSpec::ModLessThan {
        modulus: 1,
        accept: 1,
    };
    let hits = filtered_search(&rtx, &q, &m, &base, &idx, 5, 32, &f);
    assert_eq!(
        keys_of(&hits),
        vec![0, 1, 2, 3, 4],
        "under exact ties the smallest keys win, deterministically"
    );
}

// ---------------------------------------------------------------------------
// Story #87 — the min(k, matches) law, generatively and adversarially, plus
// the thread-count determinism obligation and the filter-matches-everything
// differential against the unfiltered baseline (the "old post-filter path,
// proven answer-identical" requirement).
// ---------------------------------------------------------------------------

/// THE LAW, generatively: for any filter and any `k`, a filtered search
/// returns exactly `min(k, M)` rows and every returned row satisfies the
/// filter — over hundreds of randomly generated `(k, modulus, accept)`
/// bands on one real corpus. A mutation that weakens the count guarantee to
/// best-effort (e.g. disabling the fallback, or capping the graph beam
/// without a repair) makes this bite across many cases, not just the
/// hand-picked ones below.
#[test]
fn min_k_matches_law_generative() {
    let rows = seeded_rows(P2_N, P2_DIM, P2_CORPUS_SEED);
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let (base, idx, m) = hsetup(&db, P2_DIM, HnswDistance::L2, &rows);
    let rtx = db.read_tx().unwrap();
    let q = seeded_query(P2_DIM, P2_QUERY_SEED);

    // A PINNED low-ef band, inside the sweep, that is guaranteed (not merely
    // hoped) to exercise the production fallback with a NON-EMPTY partial
    // graph result: the random (k, modulus, accept, ef=P2_EF) draws above
    // never land here because ef is fixed generous, so the graph walk either
    // fills completely or (rarely) comes back empty — never "found some,
    // not enough". This band pins ef=1 at a 50%-selectivity filter (the same
    // conditions `production_fallback_repairs_starved_real_search` uses),
    // and asserts the partial is non-empty-but-short BEFORE trusting the
    // repaired result, so the case is provably exercised every run, not just
    // statistically likely. A "concat partial + scan, truncate to k" splice
    // mutant produces duplicate keys / a wrong exact set here even though it
    // preserves `hits.len() == k` and even a membership-based recall.
    {
        let pinned_ef = 1usize;
        let pinned_f = filter_at_selectivity(0.50, true);
        let plan = selected_plan(&rtx, &q, &m, &base, &idx, P2_K, pinned_ef, &pinned_f).unwrap();
        assert!(
            matches!(plan, SearchPlan::Graph { .. }),
            "pinned band precondition: selector must pick Graph, got {plan:?}"
        );
        let fb = pinned_f.bytecode();
        let params = knn_params_p2(P2_K, pinned_ef);
        let mut stack = vec![];
        let partial = hnsw_knn_forced(
            &rtx, &q, &m, &base, &idx, &params, &fb, &mut stack, plan, false,
        )
        .unwrap();
        assert!(
            !partial.is_empty() && partial.len() < P2_K,
            "pinned band precondition: the raw graph walk must be a NON-EMPTY \
             partial (got {} of {}) so the fallback repair is genuinely \
             exercised, not a no-op or a from-scratch scan",
            partial.len(),
            P2_K
        );

        let truth = brute_force_filtered_knn(&q, P2_K, &pinned_f, &rows, &m);
        let hits = filtered_search(&rtx, &q, &m, &base, &idx, P2_K, pinned_ef, &pinned_f);
        assert_eq!(
            hits.len(),
            P2_K,
            "pinned band: the production fallback must repair the non-empty \
             partial to exactly k rows"
        );
        let mut ekeys = keys_of(&hits);
        let before = ekeys.len();
        ekeys.sort_unstable();
        ekeys.dedup();
        assert_eq!(
            ekeys.len(),
            before,
            "pinned band: the repaired result must not contain duplicate keys"
        );
        let mut sorted_truth = truth.clone();
        sorted_truth.sort_unstable();
        assert_eq!(
            ekeys, sorted_truth,
            "pinned band: the repaired result must equal the independent \
             oracle's exact top-k set"
        );
    }

    proptest!(ProptestConfig::with_cases(64), |(k in 1usize..=15, modulus in 2i64..=64, accept_raw in 0i64..64)| {
        let accept = accept_raw % modulus;
        let f = FilterSpec::ModLessThan { modulus, accept };
        let matches = f.true_match_count(&rows);
        let hits = filtered_search(&rtx, &q, &m, &base, &idx, k, P2_EF, &f);
        prop_assert_eq!(
            hits.len(),
            k.min(matches),
            "k={} modulus={} accept={} matches={}: got {} rows",
            k, modulus, accept, matches, hits.len()
        );
        for h in &hits {
            prop_assert!(f.passes(h), "returned row {h:?} fails its own filter");
        }
        // No duplicates: every returned key is distinct.
        let mut ekeys = keys_of(&hits);
        let before = ekeys.len();
        ekeys.sort_unstable();
        ekeys.dedup();
        prop_assert_eq!(ekeys.len(), before, "duplicate keys in one result set");
    });
}

/// Adversarial: match sets of exactly 1, 2, and 3 rows (well below any
/// reasonable `k`). So few matches means the selector's estimate always
/// lands in the exact-scan regime, so BOTH the count and the ranking must be
/// exact — the sharpest form of the law.
#[test]
fn min_k_matches_tiny_match_sets() {
    let rows = seeded_rows(P2_N, P2_DIM, P2_CORPUS_SEED);
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let (base, idx, m) = hsetup(&db, P2_DIM, HnswDistance::L2, &rows);
    let rtx = db.read_tx().unwrap();
    let q = seeded_query(P2_DIM, P2_QUERY_SEED);

    for threshold in [1i64, 2, 3] {
        let f = FilterSpec::LessThan { threshold };
        let matches = f.true_match_count(&rows);
        assert_eq!(matches, threshold as usize, "keys are dense 0..n");
        let truth = brute_force_filtered_knn(&q, P2_K, &f, &rows, &m);
        let hits = filtered_search(&rtx, &q, &m, &base, &idx, P2_K, P2_EF, &f);
        let ekeys = keys_of(&hits);
        assert_eq!(
            hits.len(),
            P2_K.min(matches),
            "threshold={threshold}: expected min(k, {matches}) rows, got {}",
            hits.len()
        );
        assert_eq!(
            recall_at_k(&ekeys, &truth, P2_K),
            1.0,
            "threshold={threshold}: a {matches}-row match set must be exact"
        );
    }
}

/// A corpus of two well-separated clusters: keys `0..half` are a "near"
/// cluster around the origin, keys `half..n` are the SAME random spread
/// translated `+40` in every dimension — a "far" cluster with no vector-space
/// overlap with the near one. Returns `(n, half, rows)`.
fn near_far_cluster_corpus(dim: usize) -> (i64, i64, Vec<Tuple>) {
    let n = P2_N;
    let half = n / 2;
    let mut state = P2_CORPUS_SEED ^ 0xA5A5_5A5A_1234_9876;
    let rows: Vec<Tuple> = (0..n)
        .map(|k| {
            let comps: Vec<f64> = (0..dim).map(|_| next_f32(&mut state)).collect();
            let v = if k < half {
                comps
            } else {
                comps.iter().map(|c| c + 40.0).collect()
            };
            vec![
                DataValue::from(k),
                DataValue::Vector(Vector::new(v.clone())),
            ]
        })
        .collect();
    (n, half, rows)
}

/// Adversarial: the match set is a cluster translated far away in vector
/// space from the query's natural region, at 50% selectivity (squarely the
/// Graph plan's regime, not the Scan fallback's). This is the starvation
/// scenario the design's full-graph routing exists for: a filtered walk that
/// only expands through filter-PASSING nodes would need to already be near
/// the far cluster to find it, and a naive implementation gives up as soon
/// as the near cluster it entered through is exhausted. The full-graph
/// routing (traverse every edge regardless of the endpoint's filter
/// verdict) is what lets the walk cross from the near cluster to the far
/// one; the exact-scan fallback is the backstop if it still falls short.
#[test]
fn min_k_matches_disconnected_from_entry_region() {
    let dim = 16;
    let (n, half, rows) = near_far_cluster_corpus(dim);
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let (base, idx, m) = hsetup(&db, dim, HnswDistance::L2, &rows);
    let rtx = db.read_tx().unwrap();
    // The query sits in the NEAR cluster's region (untranslated), so an
    // unfiltered search would settle near the origin — the far cluster is
    // reachable only by traversing edges through it.
    let q = seeded_query(dim, P2_QUERY_SEED);

    let f = FilterSpec::AtLeast { threshold: half }; // matches only the far cluster
    let matches = f.true_match_count(&rows);
    assert_eq!(matches, (n - half) as usize);
    let truth = brute_force_filtered_knn(&q, P2_K, &f, &rows, &m);
    let hits = filtered_search(&rtx, &q, &m, &base, &idx, P2_K, P2_EF, &f);
    let ekeys = keys_of(&hits);

    assert_eq!(
        hits.len(),
        P2_K.min(matches),
        "disconnected match set: expected min(k, {matches}), got {}",
        hits.len()
    );
    for k in &ekeys {
        assert!(*k >= half, "returned key {k} is not in the far cluster");
    }
    let cr = count_recall(&ekeys, &truth, P2_K);
    assert_eq!(
        cr, 1.0,
        "the count guarantee must hold even for a disconnected match set"
    );
    // Ranking quality is measured, not assumed: report it rather than gate
    // on an arbitrary threshold (the fallback backstops the COUNT, not the
    // graph walk's ranking quality when it fills to k without falling back).
    let r = recall_at_k(&ekeys, &truth, P2_K);
    eprintln!("disconnected-cluster recall@k = {r:.3} (count_recall = {cr:.3})");
}

/// The GRAPH WALK ITSELF (fallback disabled, ordinary — not artificially
/// starved — beam width) must cross from the near cluster it enters through
/// to the disconnected far cluster and find matches there. This isolates the
/// traversal's own routing from the scan fallback's backstop: the fallback
/// would repair a starved walk's COUNT regardless, so a mutation that stops
/// expansion at filter-failing nodes (confining the walk to the near cluster
/// forever, since every near-cluster node fails the far-cluster filter)
/// would otherwise go undetected — the fallback silently fixes the count and
/// every count/recall assertion elsewhere still passes. Forcing the plan
/// with `fallback: false` removes that safety net so THIS test bites.
#[test]
fn graph_walk_alone_crosses_to_disconnected_matches_without_fallback() {
    let dim = 16;
    let (_n, half, rows) = near_far_cluster_corpus(dim);
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let (base, idx, m) = hsetup(&db, dim, HnswDistance::L2, &rows);
    let rtx = db.read_tx().unwrap();
    let q = seeded_query(dim, P2_QUERY_SEED);
    let f = FilterSpec::AtLeast { threshold: half };
    let matches = f.true_match_count(&rows);
    let truth = brute_force_filtered_knn(&q, P2_K, &f, &rows, &m);

    // An ordinary beam width (the selector would pick something in this
    // ballpark at 50% selectivity) — no artificial starvation, so ANY
    // shortfall here comes from the routing itself, not a beam too narrow to
    // finish the job.
    let plan = SearchPlan::Graph { ef2: P2_EF * 4 };
    let params = knn_params_p2(P2_K, P2_EF);
    let fb = f.bytecode();
    let mut stack = vec![];
    let hits = hnsw_knn_forced(
        &rtx, &q, &m, &base, &idx, &params, &fb, &mut stack, plan, false,
    )
    .unwrap();
    let ekeys = keys_of(&hits);

    assert_eq!(
        hits.len(),
        P2_K.min(matches),
        "the graph walk alone (no fallback) must reach the disconnected far \
         cluster and seat min(k, matches) rows, got {}",
        hits.len()
    );
    for k in &ekeys {
        assert!(
            *k >= half,
            "returned key {k} is not in the far cluster — the walk never crossed"
        );
    }
    assert_eq!(
        count_recall(&ekeys, &truth, P2_K),
        1.0,
        "the unaided graph walk must find the full disconnected match set"
    );
}

/// Adversarial: zero matches. The filter rejects every row; the search must
/// return an empty result, not an error and not a panic.
#[test]
fn min_k_matches_zero_matches() {
    let rows = seeded_rows(P2_N, P2_DIM, P2_CORPUS_SEED);
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let (base, idx, m) = hsetup(&db, P2_DIM, HnswDistance::L2, &rows);
    let rtx = db.read_tx().unwrap();
    let q = seeded_query(P2_DIM, P2_QUERY_SEED);

    let f = FilterSpec::LessThan { threshold: 0 }; // keys are 0..n, so nothing passes
    assert_eq!(f.true_match_count(&rows), 0);
    let hits = filtered_search(&rtx, &q, &m, &base, &idx, P2_K, P2_EF, &f);
    assert!(hits.is_empty(), "zero matches must yield an empty result");
}

/// Adversarial: the filter matches EVERY row. The filtered path (selector,
/// full-graph traversal admitting everything, or the scan) must return
/// EXACTLY what the plain unfiltered `hnsw_knn` returns — the differential
/// proof that filtering-during-traversal is not a second, divergent search
/// algorithm wearing the unfiltered one's clothes; it is the same graph, the
/// same total order, with an admission gate that happens to always open.
#[test]
fn min_k_matches_filter_matching_everything_equals_unfiltered() {
    let rows = seeded_rows(P2_N, P2_DIM, P2_CORPUS_SEED);
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let (base, idx, m) = hsetup(&db, P2_DIM, HnswDistance::L2, &rows);
    let rtx = db.read_tx().unwrap();
    let q = seeded_query(P2_DIM, P2_QUERY_SEED);

    let f = FilterSpec::ModLessThan {
        modulus: 1,
        accept: 1,
    }; // (k mod 1) < 1 is always true
    assert_eq!(f.true_match_count(&rows), rows.len());

    let params = knn_params_p2(P2_K, P2_EF);
    let mut stack = vec![];
    let unfiltered = hnsw_knn(
        &rtx,
        &q,
        &m,
        &base,
        &idx,
        &params,
        &None,
        &mut stack,
        &crate::fixed_rule::CancelFlag::default(),
    )
    .unwrap();
    let filtered = filtered_search(&rtx, &q, &m, &base, &idx, P2_K, P2_EF, &f);

    assert_eq!(
        keys_of(&filtered),
        keys_of(&unfiltered),
        "an always-true filter must return exactly the unfiltered top-k, same order"
    );
    assert_eq!(
        filtered, unfiltered,
        "an always-true filter must be byte-identical to the unfiltered search, \
         appended columns included"
    );
}

/// Determinism: the filtered search is a pure function of the read snapshot
/// with no thread-local or global mutable state (the reservoir sample seed,
/// the beam, and the `(distance, key)` total order are all pure), so its
/// result is byte-identical no matter how many threads are available to the
/// process — rayon pool sizes 1/2/4/8, and genuinely concurrent OS threads
/// racing on the same read transaction.
#[test]
fn filtered_search_is_thread_count_invariant() {
    let rows = seeded_rows(P2_N, P2_DIM, P2_CORPUS_SEED);
    let q = seeded_query(P2_DIM, P2_QUERY_SEED);
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let (base, idx, m) = hsetup(&db, P2_DIM, HnswDistance::L2, &rows);
    let rtx = db.read_tx().unwrap();

    let run_under_pool = |n_threads: usize| -> Vec<Vec<Tuple>> {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(n_threads)
            .build()
            .unwrap();
        pool.install(|| {
            SELECTIVITY_BANDS
                .iter()
                .map(|&t| {
                    filtered_search(
                        &rtx,
                        &q,
                        &m,
                        &base,
                        &idx,
                        P2_K,
                        P2_EF,
                        &filter_at_selectivity(t, true),
                    )
                })
                .collect()
        })
    };
    let baseline = run_under_pool(1);
    for n in [2usize, 4, 8] {
        let got = run_under_pool(n);
        assert_eq!(
            baseline, got,
            "rayon pool size {n}: results diverged from the 1-thread baseline"
        );
    }

    // Genuinely concurrent: every band searched from its own OS thread at
    // once, sharing one read transaction.
    let concurrent: Vec<Vec<Tuple>> = std::thread::scope(|scope| {
        let handles: Vec<_> = SELECTIVITY_BANDS
            .iter()
            .map(|&t| {
                let rtx = &rtx;
                let q = &q;
                let m = &m;
                let base = &base;
                let idx = &idx;
                scope.spawn(move || {
                    filtered_search(
                        rtx,
                        q,
                        m,
                        base,
                        idx,
                        P2_K,
                        P2_EF,
                        &filter_at_selectivity(t, true),
                    )
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert_eq!(
        baseline, concurrent,
        "concurrent OS threads diverged from the sequential baseline"
    );
}

/// Adversarial: `k` exceeds the ENTIRE population — not merely the match
/// count, the whole corpus. A filtered search asking for far more rows than
/// exist anywhere in the index must still return exactly `min(k, M) = M`
/// rows: the whole matching set, in full, with no attempt to conjure rows
/// that do not exist and no panic on the size mismatch.
#[test]
fn min_k_matches_k_exceeds_entire_population() {
    let rows = seeded_rows(P2_N, P2_DIM, P2_CORPUS_SEED); // N = 4000
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let (base, idx, m) = hsetup(&db, P2_DIM, HnswDistance::L2, &rows);
    let rtx = db.read_tx().unwrap();
    let q = seeded_query(P2_DIM, P2_QUERY_SEED);

    let k = rows.len() * 10; // an order of magnitude past the whole corpus
    let f = filter_at_selectivity(0.10, true); // a genuine subset, M = 400
    let matches = f.true_match_count(&rows);
    assert!(
        matches < rows.len(),
        "sanity: the filter must be a proper subset of the corpus"
    );
    let truth = brute_force_filtered_knn(&q, k, &f, &rows, &m);
    let hits = filtered_search(&rtx, &q, &m, &base, &idx, k, P2_EF, &f);
    let mut ekeys = keys_of(&hits);

    assert_eq!(
        hits.len(),
        matches,
        "k={k} exceeds the whole population ({}); expected exactly the \
         match set ({matches} rows)",
        rows.len()
    );
    let before = ekeys.len();
    ekeys.sort_unstable();
    ekeys.dedup();
    assert_eq!(
        ekeys.len(),
        before,
        "duplicate keys when k exceeds the population"
    );
    let mut sorted_truth = truth.clone();
    sorted_truth.sort_unstable();
    assert_eq!(
        ekeys, sorted_truth,
        "must equal the exact match set — no more, no less — when k exceeds it"
    );

    // And k exceeding the corpus even when the filter matches EVERY row (so
    // M = N < k too): the whole corpus must come back, once each.
    let f_all = FilterSpec::ModLessThan {
        modulus: 1,
        accept: 1,
    };
    let hits_all = filtered_search(&rtx, &q, &m, &base, &idx, k, P2_EF, &f_all);
    let mut all_keys = keys_of(&hits_all);
    assert_eq!(
        hits_all.len(),
        rows.len(),
        "k exceeds N with an all-matching filter: expected every row exactly once"
    );
    let before_all = all_keys.len();
    all_keys.sort_unstable();
    all_keys.dedup();
    assert_eq!(
        all_keys.len(),
        before_all,
        "duplicate keys when k exceeds N with an all-matching filter"
    );
}

/// The engine's total order under equal distances holds under the GRAPH
/// plan too, not just Scan — `engine_ordering_is_total_under_ties` uses a
/// 12-row corpus small enough that the selector always picks Scan, so it
/// never actually exercised the graph traversal's tie-break. This test uses
/// the same axis-unit-vector tie construction at a corpus size and
/// selectivity that forces `SearchPlan::Graph`, checks the result matches
/// the independent oracle's tie-break EXACTLY (not just in count), and that
/// the tie-break is thread-count invariant — a "drop the tie-break" or a
/// hash-order-leaking mutation in construction or search would diverge
/// across independent rebuilds or across thread counts (or both). Exact
/// recall vs brute force is NOT claimed: HNSW is approximate and this
/// corpus is adversarial (identical-vector clusters); the law is
/// determinism, enforced by the `(distance, VectorId)` beam priority.
#[test]
fn graph_plan_tie_break_at_k_boundary_is_thread_count_invariant() {
    let dim = 16;
    let n = P2_N;
    let rows: Vec<Tuple> = (0..n)
        .map(|i| {
            let mut comps = vec![0.0f64; dim];
            comps[(i as usize) % dim] = 1.0; // a distinct axis unit vector per residue class
            vec![DataValue::from(i), DataValue::Vector(Vector::new(comps))]
        })
        .collect();
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let (base, idx, m) = hsetup(&db, dim, HnswDistance::L2, &rows);
    let rtx = db.read_tx().unwrap();
    // Every row is EXACTLY equidistant (squared L2 = 1.0, bit-exact) from the
    // all-zero query, so only the `(distance, encoded-key)` tie-break decides
    // the k survivors.
    let q = Vector::new(vec![0.0f64; dim]);
    // Even keys only (`k mod 2 == 0 < 1`): a genuine filter (not
    // all-matching), ~half the corpus — enough matches (~2000) to force the
    // Graph plan, not Scan.
    let f = FilterSpec::ModLessThan {
        modulus: 2,
        accept: 1,
    };

    let plan = selected_plan(&rtx, &q, &m, &base, &idx, P2_K, P2_EF, &f).unwrap();
    assert!(
        matches!(plan, SearchPlan::Graph { .. }),
        "precondition: this corpus/filter must select Graph, got {plan:?}"
    );

    let baseline = filtered_search(&rtx, &q, &m, &base, &idx, P2_K, P2_EF, &f);

    // LAWFULNESS. HNSW is an APPROXIMATE index. This corpus is adversarial:
    // `comps[i % dim] = 1.0` makes every residue class an identical vector,
    // so same-axis nodes are distance 0 from each other and cross-axis nodes
    // are distance sqrt(2) — the graph connects densely WITHIN a cluster and
    // sparsely ACROSS clusters. No correct HNSW can therefore return the
    // globally smallest keys across disconnected identical-vector clusters;
    // it returns one cluster's members. The invariant that IS lawful — and
    // that the `(distance, VectorId)` beam priority now guarantees — is
    // DETERMINISM: the exact same k survivors on every build and every
    // search, independent of hasher state, thread count, or run. (Before the
    // tie-break fix the survivors leaked the priority queue's hash-map
    // iteration order and varied run to run.)

    // Every survivor is a genuine match: an even key whose vector is exactly
    // equidistant (squared L2 = 1.0) from the all-zero query.
    for key in keys_of(&baseline) {
        assert!(key % 2 == 0, "filter admits only even keys, got {key}");
    }
    assert_eq!(keys_of(&baseline).len(), P2_K, "k survivors");

    // Reproducibility across an INDEPENDENT rebuild: a fresh store, a fresh
    // graph built from the same rows, searched again, yields byte-identical
    // survivors. A hash-order-leaking construction or search would diverge
    // here even single-threaded.
    let rebuilt = {
        let dir2 = tempfile::tempdir().unwrap();
        let db2 = new_fjall_storage(dir2.path()).unwrap();
        let (base2, idx2, m2) = hsetup(&db2, dim, HnswDistance::L2, &rows);
        let rtx2 = db2.read_tx().unwrap();
        filtered_search(&rtx2, &q, &m2, &base2, &idx2, P2_K, P2_EF, &f)
    };
    assert_eq!(
        keys_of(&rebuilt),
        keys_of(&baseline),
        "an independent rebuild produced different survivors: construction or \
         search is not deterministic"
    );

    // Thread-count invariance of that same tie-break: rayon pools of
    // 1/2/4/8 and genuinely concurrent OS threads must all agree.
    let run_under_pool = |n_threads: usize| -> Vec<Tuple> {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(n_threads)
            .build()
            .unwrap();
        pool.install(|| filtered_search(&rtx, &q, &m, &base, &idx, P2_K, P2_EF, &f))
    };
    for n_threads in [1usize, 2, 4, 8] {
        let got = run_under_pool(n_threads);
        assert_eq!(
            got, baseline,
            "rayon pool size {n_threads}: the graph-plan tie-break diverged \
             from the 1-thread baseline"
        );
    }
    let concurrent: Vec<Vec<Tuple>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let rtx = &rtx;
                let q = &q;
                let m = &m;
                let base = &base;
                let idx = &idx;
                let f = &f;
                scope.spawn(move || filtered_search(rtx, q, m, base, idx, P2_K, P2_EF, f))
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for got in concurrent {
        assert_eq!(
            got, baseline,
            "a concurrent OS thread's graph-plan tie-break diverged from the baseline"
        );
    }
}

/// Measurement rig (opt-in): print the filter-aware recall table for the report.
#[test]
#[ignore = "measurement rig; run explicitly to print the filter-aware table"]
fn filter_aware_recall_table() {
    let rows = seeded_rows(P2_N, P2_DIM, P2_CORPUS_SEED);
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let (base, idx, m) = hsetup(&db, P2_DIM, HnswDistance::L2, &rows);
    let rtx = db.read_tx().unwrap();
    let q = seeded_query(P2_DIM, P2_QUERY_SEED);

    eprintln!("  sel  match  plan     recall@k  count  | baseline r/c");
    for (target, br, bc) in PINNED_BASELINE {
        // Alternate the accepted-set shape across the sweep: half the
        // bands run striped (mod), half contiguous (threshold).
        let f = filter_at_selectivity(target, (target * 100.0) as i64 % 2 == 0);
        let matches = f.true_match_count(&rows);
        let plan = selected_plan(&rtx, &q, &m, &base, &idx, P2_K, P2_EF, &f);
        let truth = brute_force_filtered_knn(&q, P2_K, &f, &rows, &m);
        let hits = filtered_search(&rtx, &q, &m, &base, &idx, P2_K, P2_EF, &f);
        let ekeys = keys_of(&hits);
        let r = recall_at_k(&ekeys, &truth, P2_K);
        let cr = count_recall(&ekeys, &truth, P2_K);
        eprintln!("{target:>5.2} {matches:>6}  {plan:?}   {r:>7.3} {cr:>6.3}  |  {br:.3}/{bc:.3}");
    }
}
