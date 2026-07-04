//! HOSTILE REVIEW tests for the sparse-vector engine — NOT part of the reviewed
//! module. Independently-written references and adversarial scenarios: argument-
//! order and insertion-order byte-identity (the summation-order law the module's
//! own test under-pins because it feeds a pre-sorted query), large tie sets,
//! denormal/huge weights, and every admission path.

#![cfg(test)]

use std::collections::BTreeMap;

use crate::data::program::InputRelationHandle;
use crate::data::relation::{ColType, ColumnDef, NullableColType, StoredRelationMetadata};
use crate::data::span::SourceSpan;
use crate::data::symb::Symbol;
use crate::data::value::DataValue;
use crate::engines::sparse::{
    SparseSearchParams, sparse_index_metadata, sparse_put, sparse_search, sparse_total_docs,
};
use crate::runtime::relation::{KeyspaceKind, RelationHandle, create_relation};
use crate::storage::fjall::new_fjall_storage;
use crate::storage::{Storage, WriteTx};
use smartstring::SmartString;

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

fn base_meta() -> StoredRelationMetadata {
    StoredRelationMetadata {
        keys: vec![col("k", ColType::Int)],
        non_keys: vec![col("tag", ColType::String)],
    }
}

struct Fixture {
    base: RelationHandle,
    idx: RelationHandle,
}

type Doc<'a> = (i64, &'a str, &'a [(u32, f32)]);

fn setup(db: &impl Storage, docs: &[Doc]) -> Fixture {
    let meta = base_meta();
    let mut tx = db.write_tx().unwrap();
    let base = create_relation(
        &mut tx,
        input_handle("docs", meta.clone()),
        KeyspaceKind::Facts,
    )
    .unwrap();
    let idx = create_relation(
        &mut tx,
        input_handle("docs:sparse", sparse_index_metadata(&meta)),
        KeyspaceKind::AlgorithmState,
    )
    .unwrap();
    for (k, tag, vector) in docs {
        let row = vec![DataValue::from(*k), DataValue::from(*tag)];
        base.put_fact(
            &mut tx,
            &row,
            crate::data::value::ValidityTs(std::cmp::Reverse(0)),
            SourceSpan(0, 0),
        )
        .unwrap();
        sparse_put(&mut tx, &row, vector, &base, &idx).unwrap();
    }
    tx.commit().unwrap();
    Fixture { base, idx }
}

fn params(k: usize) -> SparseSearchParams {
    SparseSearchParams {
        k,
        bind_score: true,
    }
}

/// Run and project (key, score-bits) so we can compare EXACT f32 bit patterns.
fn run_bits(db: &impl Storage, f: &Fixture, query: &[(u32, f32)], k: usize) -> Vec<(i64, u32)> {
    let rtx = db.read_tx().unwrap();
    let mut stack = vec![];
    let hits = sparse_search(&rtx, query, &f.base, &f.idx, &params(k), &None, &mut stack).unwrap();
    hits.iter()
        .map(|t| {
            (
                t[0].get_int().unwrap(),
                (t.last().unwrap().get_float().unwrap() as f32).to_bits(),
            )
        })
        .collect()
}

/// The score determinism law says a document's score is byte-identical
/// regardless of the ORDER the query pairs are supplied in (admission sorts
/// them). Feed the SAME logical query in ascending, descending, and shuffled
/// order and demand identical score bit patterns. The module's own
/// `summation_order_is_pinned` feeds an already-ascending query, so it does NOT
/// exercise the engine's sort; this does.
#[test]
fn query_argument_order_is_irrelevant_to_score_bits() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    // The order-sensitive f32 construction from the module's own test.
    let big = 16_777_216.0f32; // 2^24
    let docs: &[Doc] = &[(1, "x", &[(1, 1.0), (2, 1.0), (3, big)])];
    let f = setup(&db, docs);

    let ascending = &[(1, 1.0f32), (2, 1.0f32), (3, 1.0f32)];
    let descending = &[(3, 1.0f32), (2, 1.0f32), (1, 1.0f32)];
    let shuffled = &[(2, 1.0f32), (3, 1.0f32), (1, 1.0f32)];

    let a = run_bits(&db, &f, ascending, 1);
    let d = run_bits(&db, &f, descending, 1);
    let s = run_bits(&db, &f, shuffled, 1);
    assert_eq!(a, d, "descending query must produce identical score bits");
    assert_eq!(a, s, "shuffled query must produce identical score bits");
    // And it is the ascending-order f32 sum (2^24 + 2), not the descending one.
    assert_eq!(
        a[0].1,
        16_777_218.0f32.to_bits(),
        "score is the ascending-order sum"
    );
}

/// Byte-identity of scores regardless of the order documents were INSERTED —
/// postings are stored in memcmp key order, so scan order (hence summation) is
/// insertion-independent.
#[test]
fn insertion_order_is_irrelevant_to_score_bits() {
    let query = &[(1, 1.0f32), (2, 1.0f32), (3, 1.0f32)];
    let big = 16_777_216.0f32;
    let forward: &[Doc] = &[
        (1, "x", &[(1, 1.0), (2, 1.0), (3, big)]),
        (2, "y", &[(1, 2.0), (3, 4.0)]),
    ];
    let reversed: &[Doc] = &[
        (2, "y", &[(3, 4.0), (1, 2.0)]),
        (1, "x", &[(3, big), (2, 1.0), (1, 1.0)]),
    ];
    let build = |docs: &[Doc]| {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let f = setup(&db, docs);
        run_bits(&db, &f, query, 10)
    };
    assert_eq!(
        build(forward),
        build(reversed),
        "insertion order changed scores"
    );
}

/// Independent correctness reference computed in f64 (higher precision). For
/// weight sets whose f32 accumulation is exact (small integers / exact
/// binary fractions), the engine's f32 score must equal the f64 dot product.
fn naive_dot_f64(docs: &[Doc], query: &[(u32, f32)]) -> Vec<(i64, f64)> {
    let mut q = query.to_vec();
    q.sort_by_key(|&(d, _)| d);
    let mut out = vec![];
    for (key, _t, vector) in docs {
        let dmap: BTreeMap<u32, f32> = vector.iter().copied().collect();
        let mut score = 0.0f64;
        for &(qd, qw) in &q {
            if let Some(&dw) = dmap.get(&qd) {
                score += qw as f64 * dw as f64;
            }
        }
        if score > 0.0 {
            out.push((*key, score));
        }
    }
    out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap().then(a.0.cmp(&b.0)));
    out
}

#[test]
fn matches_independent_f64_reference_on_exact_weights() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    // Exact binary fractions and small ints: f32 == f64 dot product exactly.
    let docs: &[Doc] = &[
        (1, "a", &[(0, 0.5), (2, 0.25), (5, 2.0)]),
        (2, "b", &[(0, 4.0), (1, 0.125)]),
        (3, "c", &[(2, 1.5), (5, 0.75), (9, 8.0)]),
        (4, "d", &[(7, 1.0)]),
    ];
    let f = setup(&db, docs);
    let query = &[(0, 2.0f32), (2, 4.0f32), (5, 0.5f32), (9, 0.25f32)];
    let rtx = db.read_tx().unwrap();
    let mut stack = vec![];
    let hits = sparse_search(&rtx, query, &f.base, &f.idx, &params(10), &None, &mut stack).unwrap();
    let got: Vec<(i64, f64)> = hits
        .iter()
        .map(|t| {
            (
                t[0].get_int().unwrap(),
                t.last().unwrap().get_float().unwrap(),
            )
        })
        .collect();
    let want = naive_dot_f64(docs, query);
    assert_eq!(got.len(), want.len());
    for (g, w) in got.iter().zip(want.iter()) {
        assert_eq!(g.0, w.0, "key order matches f64 reference");
        assert_eq!(g.1, w.1, "exact-weight score equals f64 dot product");
    }
}

/// k+2 candidates all tied at the SAME score; pin the surviving keys and the
/// truncation to k (the k lowest keys by memcmp order).
#[test]
fn large_tie_set_topk_survivors_pinned() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let docs: &[Doc] = &[
        (50, "e", &[(0, 1.0)]),
        (10, "a", &[(0, 1.0)]),
        (40, "d", &[(0, 1.0)]),
        (20, "b", &[(0, 1.0)]),
        (30, "c", &[(0, 1.0)]),
    ];
    let f = setup(&db, docs);
    let query = &[(0, 1.0f32)];
    let all = run_bits(&db, &f, query, 10);
    assert_eq!(
        all.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
        vec![10, 20, 30, 40, 50],
        "ties break on ascending key"
    );
    // All share one score bit-pattern.
    let s0 = all[0].1;
    assert!(all.iter().all(|(_, s)| *s == s0));
    // Truncate to k=3 -> three lowest keys.
    let k3 = run_bits(&db, &f, query, 3);
    assert_eq!(
        k3.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
        vec![10, 20, 30]
    );
}

/// Denormal and very-large finite weights: admitted (finite, non-negative),
/// deterministic, and never a NaN. A product overflowing to +inf is a valid
/// (non-NaN) score.
#[test]
fn denormal_and_huge_weights_are_finite_deterministic() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let tiny = f32::from_bits(1); // smallest positive subnormal
    let huge = f32::MAX;
    let docs: &[Doc] = &[
        (1, "a", &[(0, tiny), (1, 1.0)]),
        (2, "b", &[(0, huge), (1, huge)]),
    ];
    let f = setup(&db, docs);
    // Query with tiny and huge weights; run twice, demand identical bits.
    let query = &[(0, huge), (1, tiny)];
    let a = run_bits(&db, &f, query, 10);
    let b = run_bits(&db, &f, query, 10);
    assert_eq!(a, b, "huge/denormal weights are deterministic");
    for (_, bits) in &a {
        let v = f32::from_bits(*bits);
        assert!(!v.is_nan(), "score is never NaN (got bits {bits:#x})");
    }
    // Doc 2: huge*huge overflows to +inf — a hit, and +inf, not NaN.
    let doc2 = a.iter().find(|(k, _)| *k == 2).unwrap();
    assert_eq!(
        f32::from_bits(doc2.1),
        f32::INFINITY,
        "huge*huge overflows to +inf"
    );
}

/// REVIEWER ADDITION: `sparse_total_docs` had ZERO coverage in either suite
/// (module or hostile). It counts base-relation rows via a [prefix, Bot) range;
/// pin that it counts rows (not postings) and that an empty base counts 0.
#[test]
fn total_docs_counts_base_rows_not_postings() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    // 3 docs, 6 postings total — the count must be 3.
    let docs: &[Doc] = &[
        (1, "a", &[(0, 1.0), (1, 1.0), (2, 1.0)]),
        (2, "b", &[(0, 1.0), (5, 1.0)]),
        (3, "c", &[(9, 1.0)]),
    ];
    let f = setup(&db, docs);
    let rtx = db.read_tx().unwrap();
    assert_eq!(sparse_total_docs(&rtx, &f.base).unwrap(), 3);
    drop(rtx);

    let dir2 = tempfile::tempdir().unwrap();
    let db2 = new_fjall_storage(dir2.path()).unwrap();
    let empty = setup(&db2, &[]);
    let rtx2 = db2.read_tx().unwrap();
    assert_eq!(sparse_total_docs(&rtx2, &empty.base).unwrap(), 0);
}

/// `k == 0` must bound the filtered path exactly like the unfiltered one:
/// zero rows. The loop used to check `ret.len() >= k` AFTER pushing a
/// candidate, so a filter-present search with k=0 returned one row instead
/// of zero (the unfiltered path never showed it: it truncates `result` to
/// `k` up front and skips the loop body entirely at k=0). Fixed by checking
/// before pushing, in both this engine and the identical shape in FTS
/// (`engines/fts.rs::fts_search`).
#[test]
fn k_zero_filter_path_returns_zero_rows() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let docs: &[Doc] = &[(1, "a", &[(0, 1.0)])];
    let f = setup(&db, docs);
    let rtx = db.read_tx().unwrap();
    let mut stack = vec![];
    // Always-true filter: the constant `true`.
    let filter = vec![crate::data::expr::Bytecode::Const {
        val: DataValue::from(true),
        span: SourceSpan(0, 0),
    }];
    let p = SparseSearchParams {
        k: 0,
        bind_score: false,
    };
    let with_filter = sparse_search(
        &rtx,
        &[(0, 1.0)],
        &f.base,
        &f.idx,
        &p,
        &Some((filter, SourceSpan(0, 0))),
        &mut stack,
    )
    .unwrap();
    assert!(
        with_filter.is_empty(),
        "k=0 + filter must return 0 rows, got {}",
        with_filter.len()
    );
    let without = sparse_search(&rtx, &[(0, 1.0)], &f.base, &f.idx, &p, &None, &mut stack).unwrap();
    assert!(without.is_empty(), "k=0 without filter returns 0 rows");
}

/// -0.0 is admitted (it is not < 0.0) and never becomes a hit.
#[test]
fn negative_zero_weight_admitted_and_never_hits() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let docs: &[Doc] = &[(1, "a", &[(0, -0.0f32), (1, 1.0)])];
    let f = setup(&db, docs);
    // Query dim 0 alone: contribution is q*(-0.0) = 0 -> not a hit.
    assert!(
        run_bits(&db, &f, &[(0, 5.0)], 10).is_empty(),
        "-0.0 weight never hits"
    );
    // Query dim 1: real hit.
    assert_eq!(run_bits(&db, &f, &[(1, 1.0)], 10).len(), 1);
}
