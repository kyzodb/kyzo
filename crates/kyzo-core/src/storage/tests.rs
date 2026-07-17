/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Contract tests for the storage kernel.
//!
//! The encoding tests are LAWS, not scenarios: each is a universal property
//! quantified over all values, because the failure modes that matter here
//! (cross-type tag disorder, non-monotone float encodings, NaN order
//! divergence) are invisible to example-based tests:
//!
//! - **Law 1 (round-trip)**: decode(encode(v)) == v, under semantic equality.
//! - **Law 2 (order embedding)**: encode(a) cmp encode(b) == a cmp b, for ALL
//!   pairs — cross-type pairs included. Checked exhaustively over a corpus
//!   and generatively over arbitrary values.
//! - **Law 3 (no panic on corrupt input)**: decoding arbitrary bytes returns
//!   an error or a value, never panics.
//!
//! The storage tests use oracles: the KV contract is checked against a
//! BTreeMap model executing the same operations, and the seek-based
//! time-travel scan is checked against a naive full-scan reference
//! implementation of the as-of semantics.

use std::collections::BTreeMap;

use fjall::Slice;
use proptest::prelude::*;
use smartstring::{LazyCompact, SmartString};

use crate::data::bitemporal::ClaimPolarity;
use crate::data::relation::StoredRelationMetadata;
use crate::data::value::{AsOf, Bound, DataValue, Interval, Num, Validity, ValidityTs, Vector};
use crate::data::value::{StorageKey, RelationId, Tuple, TupleT};
use crate::runtime::relation::{AccessLevel, KeyspaceKind, RelationHandle, SystemKey};
use crate::storage::backup::{DumpClockFloorViolation, dump_storage, restore_storage};
use crate::storage::fjall::new_fjall_storage;
use crate::storage::{ReadTx, Storage, WriteTx};

// ---------- encoding laws ----------

/// Corpus rules: every `DataValue` variant appears; every variant has at
/// least two members so cross-type AND within-type pairs exist; known-tricky
/// regions (float/int ties, negative floats in vectors, NaN, empty
/// collections, unicode, length-vs-content vector ordering) are represented.
/// Adding a case is one line.
fn corpus() -> Vec<DataValue> {
    let mut c = vec![
        DataValue::Null,
        DataValue::Bool(false),
        DataValue::Bool(true),
        DataValue::Num(Num::float(f64::NEG_INFINITY)),
        DataValue::Num(Num::int(i64::MIN)),
        DataValue::Num(Num::int(-1_000_000)),
        DataValue::Num(Num::float(-1.5)),
        DataValue::Num(Num::int(-1)),
        DataValue::Num(Num::float(-0.0)),
        DataValue::Num(Num::int(0)),
        DataValue::Num(Num::float(0.0)),
        DataValue::Num(Num::float(0.5)),
        DataValue::Num(Num::int(1)),
        DataValue::Num(Num::float(1.0)), // int/float tie
        DataValue::Num(Num::float(1.5)),
        DataValue::Num(Num::int(2)),
        // The exact-int boundary: 2^53 ± 1 in both representations.
        DataValue::Num(Num::int((1 << 53) - 1)),
        DataValue::Num(Num::int(1 << 53)),
        DataValue::Num(Num::int((1 << 53) + 1)),
        DataValue::Num(Num::float((1u64 << 53) as f64)),
        DataValue::Num(Num::int(i64::MAX)),
        DataValue::Num(Num::float(f64::INFINITY)),
        DataValue::Num(Num::float(f64::NAN)),
        DataValue::Str("".into()),
        DataValue::Str("a".into()),
        DataValue::Str("ab".into()),
        DataValue::Str("b".into()),
        DataValue::Str("Ω unicode ω".into()),
        DataValue::Bytes(vec![]),
        DataValue::Bytes(vec![0]),
        DataValue::Bytes(vec![0, 1]),
        DataValue::Bytes(vec![255]),
        DataValue::Uuid(crate::UuidWrapper::new(uuid::Uuid::from_u128(0))),
        DataValue::Uuid(crate::UuidWrapper::new(uuid::Uuid::from_u128(
            0x1234_5678_9abc_def0_1234_5678_9abc_def0,
        ))),
        DataValue::Regex(
            crate::data::value::RegexSource::validated(
                crate::data::value::RegexFlags::NONE,
                "^a.*b$".into(),
            )
            .unwrap(),
        ),
        DataValue::Regex(
            crate::data::value::RegexSource::validated(
                crate::data::value::RegexFlags::NONE,
                "^z+$".into(),
            )
            .unwrap(),
        ),
        DataValue::List(vec![]),
        DataValue::List(vec![DataValue::Num(Num::int(1))]),
        // prefix ordering: [1] < [1, "x"]
        DataValue::List(vec![
            DataValue::Num(Num::int(1)),
            DataValue::Str("x".into()),
        ]),
        DataValue::Set(Default::default()),
        DataValue::Set(
            [DataValue::Num(Num::int(1)), DataValue::Num(Num::int(2))]
                .into_iter()
                .collect(),
        ),
        // Vectors: negative values (raw-bit ordering breaks here), length
        // before content, both element widths.
        DataValue::Vector(Vector::new(vec![-2.5f64, 0.0, 1.0])),
        DataValue::Vector(Vector::new(vec![-2.5f64, 0.5, 1.0])),
        DataValue::Vector(Vector::new(vec![1.0f64])),
        DataValue::Vector(Vector::new(vec![f64::NAN])),
        // Signed zero: `OrderedFloat` treats -0.0 == 0.0 (unlike scalar
        // `Num`, which distinguishes them below), so these two must encode
        // byte-identically — see `law_vector_signed_zero_canonicalizes`.
        DataValue::Vector(Vector::new(vec![-0.0f64])),
        DataValue::Vector(Vector::new(vec![0.0f64])),
        DataValue::Vector(Vector::new(vec![-7.5f64])),
        DataValue::Vector(Vector::new(vec![0.25f64, -7.5])),
        DataValue::Vector(Vector::new(vec![-0.0f64])),
        DataValue::Vector(Vector::new(vec![0.0f64])),
        DataValue::Json(crate::data::json::json_from_serde(
            &serde_json::json!({"a": 1}),
        )),
        DataValue::Json(crate::data::json::json_from_serde(&serde_json::json!([
            1, 2, 3
        ]))),
        DataValue::Validity(Validity::new(ValidityTs::from_raw(42), true).expect("non-reserved")),
        DataValue::Validity(
            Validity::new(ValidityTs::from_raw(42), false).expect("retract admits every tick"),
        ),
        DataValue::Validity(Validity::new(ValidityTs::from_raw(41), true).expect("non-reserved")),
        // Intervals: the i64::MIN/MAX boundaries, adjacent pairs (meets: end
        // of one equals start of the next), an overlapping pair, and an
        // open-ended interval (end == i64::MAX, the plain-max-tick "END"
        // convention — see `Interval`'s doc comment).
        DataValue::Interval(Interval::new(
            Bound::Closed(i64::MIN),
            Bound::Closed((i64::MIN + 1) - 1),
        )),
        DataValue::Interval(Interval::new(Bound::Closed(-100), Bound::Closed((-1) - 1))),
        DataValue::Interval(Interval::new(Bound::Closed(-1), Bound::Closed((0) - 1))),
        DataValue::Interval(Interval::new(Bound::Closed(0), Bound::Closed(0))),
        DataValue::Interval(Interval::new(Bound::Closed(0), Bound::Closed((10) - 1))),
        DataValue::Interval(Interval::new(Bound::Closed(5), Bound::Closed((15) - 1))), // overlaps [0,10)
        DataValue::Interval(Interval::new(Bound::Closed(10), Bound::Closed((20) - 1))), // meets [0,10)
        DataValue::Interval(Interval::new(Bound::Closed(0), Bound::Closed((20) - 1))), // contains [5,15)
        DataValue::Interval(Interval::new(
            Bound::Closed(100),
            Bound::Closed((i64::MAX) - 1),
        )), // open-ended
        DataValue::Interval(Interval::new(
            Bound::Closed(i64::MAX - 1),
            Bound::Closed((i64::MAX) - 1),
        )),
    ];
    // Nested collections — bound by name so corpus insertions can't silently
    // change which values get nested.
    let nested_set = DataValue::Set(
        [DataValue::Num(Num::int(1)), DataValue::Num(Num::int(2))]
            .into_iter()
            .collect(),
    );
    let nested_list = DataValue::List(vec![DataValue::Num(Num::int(1))]);
    c.push(DataValue::List(vec![nested_set, nested_list]));
    c
}

fn encode(v: &DataValue) -> Vec<u8> {
    let mut buf = vec![];
    crate::data::value::append_canonical(&mut buf, v);
    buf
}

#[test]
fn law1_round_trip_corpus() {
    for v in corpus() {
        let buf = encode(&v);
        let (decoded, rest) = DataValue::decode_from_key(&buf)
            .unwrap_or_else(|e| panic!("decode failed for {v:?}: {e}"));
        assert_eq!(decoded, v, "round-trip failed for {v:?}");
        assert!(rest.is_empty(), "trailing bytes for {v:?}");
    }
}

/// Exhaustive PAIRWISE check: cross-type disagreements cannot hide behind
/// sort stability, and a failure names the exact offending pair.
#[test]
fn law2_order_embedding_corpus_pairwise() {
    let values = corpus();
    let encoded: Vec<Vec<u8>> = values.iter().map(encode).collect();
    for i in 0..values.len() {
        for j in 0..values.len() {
            let semantic = values[i].cmp(&values[j]);
            let bytes = encoded[i].cmp(&encoded[j]);
            assert_eq!(
                semantic, bytes,
                "order disagreement:\n  a = {:?}\n  b = {:?}\n  semantic: {semantic:?}, bytewise: {bytes:?}",
                values[i], values[j]
            );
        }
    }
}

/// The generative arm of the laws: arbitrary values, including the regions
/// nobody thought to put in a corpus. Regex is the ONLY excluded variant
/// (arbitrary strings are not valid patterns); the corpus covers it.
fn arb_value() -> impl Strategy<Value = DataValue> {
    let leaf = prop_oneof![
        Just(DataValue::Null),
        any::<bool>().prop_map(DataValue::Bool),
        any::<i64>().prop_map(|i| DataValue::Num(Num::int(i))),
        any::<f64>().prop_map(|f| DataValue::Num(Num::float(f))),
        "[\\PC]{0,12}".prop_map(DataValue::Str),
        // Json's Ord and encoding both reduce to the serialized string, but
        // the reduction is an argument, not a law — fuzz it like the rest.
        "[\\PC]{0,8}".prop_map(|s| DataValue::Json(crate::data::json::json_from_serde(
            &serde_json::Value::String(s)
        ))),
        proptest::collection::vec(any::<u8>(), 0..24).prop_map(DataValue::Bytes),
        any::<u128>()
            .prop_map(|u| { DataValue::Uuid(crate::UuidWrapper::new(uuid::Uuid::from_u128(u))) }),
        proptest::collection::vec(any::<f64>(), 0..6)
            .prop_map(|v| DataValue::Vector(Vector::new(v))),
        (any::<i64>(), any::<bool>()).prop_map(|(ts, a)| {
            let vts = ValidityTs::from_raw(ts);
            DataValue::Validity(
                Validity::new(vts, a).unwrap_or_else(|| Validity::from_stored(vts, a)),
            )
        }),
        // Two arbitrary i64s, ordered into a closed interval (a tie is the
        // single-instant interval, a lawful value of the kind).
        (any::<i64>(), any::<i64>()).prop_map(|(a, b)| {
            let (start, end) = if a <= b { (a, b) } else { (b, a) };
            DataValue::Interval(Interval::new(Bound::Closed(start), Bound::Closed(end)))
        }),
    ];
    leaf.prop_recursive(3, 24, 4, |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 0..4).prop_map(DataValue::List),
            proptest::collection::btree_set(inner, 0..4).prop_map(DataValue::Set),
        ]
    })
}

proptest! {
    #[test]
    fn law1_round_trip_generative(v in arb_value()) {
        let buf = encode(&v);
        let (decoded, rest) = DataValue::decode_from_key(&buf)
            .map_err(|e| TestCaseError::fail(format!("decode failed: {e}")))?;
        prop_assert_eq!(decoded, v);
        prop_assert!(rest.is_empty());
    }

    #[test]
    fn law2_order_embedding_generative(a in arb_value(), b in arb_value()) {
        let (ea, eb) = (encode(&a), encode(&b));
        prop_assert_eq!(
            a.cmp(&b), ea.cmp(&eb),
            "order disagreement between {:?} and {:?}", a, b
        );
    }

    /// Corrupt/arbitrary input must produce an error or a value — never a
    /// panic: decoding stored bytes is fallible by contract.
    #[test]
    fn law3_decode_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..64)) {
        let _ = DataValue::decode_from_key(&bytes);
    }

    /// Targeted arm of law 2 for `Interval`: two arbitrary uniformly-random
    /// intervals almost never share a boundary (`arb_value`'s two draws are
    /// independent over the full `i64` range), so that generic generator has
    /// no power against a comparison that drops one field while the other
    /// happens to tie — exactly the shape of a "compare only `start`" or
    /// "compare only `end`" bug. This arm forces the shared boundary: same
    /// `start`, two different `end`s (and separately, same `end`, two
    /// different `start`s), so a dropped-field comparator collapses a real
    /// `Less`/`Greater` into a wrong `Equal` on almost every draw.
    #[test]
    fn law2_order_embedding_shared_boundary_generative(
        start in any::<i64>(),
        end in any::<i64>(),
        delta in 1i64..=1_000_000,
    ) {
        // Guard against overflow at the extremes rather than wrapping into a
        // false failure: skip draws where `end`/`start` sit within `delta`
        // of i64's bounds.
        prop_assume!(end.checked_add(delta).is_some());
        prop_assume!(start.checked_sub(delta).is_some());
        prop_assume!(end > start);

        let same_start_short = DataValue::Interval(Interval::new(Bound::Closed(start), Bound::Closed((end) - 1)));
        let same_start_long = DataValue::Interval(Interval::new(Bound::Closed(start), Bound::Closed((end + delta) - 1)));
        prop_assert_eq!(
            same_start_short.cmp(&same_start_long),
            encode(&same_start_short).cmp(&encode(&same_start_long)),
            "same-start pair order disagreement: {:?} vs {:?}", same_start_short, same_start_long
        );

        let same_end_early = DataValue::Interval(Interval::new(Bound::Closed(start - delta), Bound::Closed((end) - 1)));
        let same_end_late = DataValue::Interval(Interval::new(Bound::Closed(start), Bound::Closed((end) - 1)));
        prop_assert_eq!(
            same_end_early.cmp(&same_end_late),
            encode(&same_end_early).cmp(&encode(&same_end_late)),
            "same-end pair order disagreement: {:?} vs {:?}", same_end_early, same_end_late
        );
    }
}

/// Signed zero in a `Vector` lane: `-0.0` and `+0.0` are semantically Equal
/// under `Vector`'s `OrderedFloat`-based `Ord`/`PartialEq` (unlike scalar
/// `Num`, which uses `total_cmp` and legitimately orders `-0.0 < +0.0` — see
/// `order_encode_f64`'s doc comment). The encoding must therefore produce
/// byte-identical, equally-ordered keys for `-0.0` and `+0.0` inside a
/// vector, exactly as it already does for the two NaN encodings.
#[test]
fn law_vector_signed_zero_canonicalizes() {
    let neg_f32 = DataValue::Vector(Vector::new(vec![-0.0f64]));
    let pos_f32 = DataValue::Vector(Vector::new(vec![0.0f64]));
    assert_eq!(neg_f32.cmp(&pos_f32), std::cmp::Ordering::Equal);
    assert_eq!(
        encode(&neg_f32),
        encode(&pos_f32),
        "-0.0 and +0.0 are semantically Equal in a Vector lane but encoded to different bytes"
    );

    let neg_f64 = DataValue::Vector(Vector::new(vec![-0.0f64]));
    let pos_f64 = DataValue::Vector(Vector::new(vec![0.0f64]));
    assert_eq!(neg_f64.cmp(&pos_f64), std::cmp::Ordering::Equal);
    assert_eq!(
        encode(&neg_f64),
        encode(&pos_f64),
        "-0.0 and +0.0 are semantically Equal in a Vector lane but encoded to different bytes"
    );

    // Mixed sign, multi-element: only the zero lane should collapse.
    let neg_mixed = DataValue::Vector(Vector::new(vec![-1.5f64, -0.0, 2.5]));
    let pos_mixed = DataValue::Vector(Vector::new(vec![-1.5f64, 0.0, 2.5]));
    assert_eq!(neg_mixed.cmp(&pos_mixed), std::cmp::Ordering::Equal);
    assert_eq!(encode(&neg_mixed), encode(&pos_mixed));
}

/// The value plane has ONE canonical zero: `Num::float(-0.0)` collapses to
/// `+0.0` at construction, so the two are the same value — equal, and
/// byte-identical under the canonical encoding. This pins that law so a
/// future change cannot re-introduce a distinct negative zero.
#[test]
fn law_scalar_num_negative_zero_collapses_to_positive() {
    let neg = DataValue::Num(Num::float(-0.0));
    let pos = DataValue::Num(Num::float(0.0));
    assert_eq!(neg.cmp(&pos), std::cmp::Ordering::Equal);
    assert_eq!(neg, pos);
    assert_eq!(encode(&neg), encode(&pos));
}

// ---------- KV contract vs a model oracle ----------

/// Ops applied identically to fjall and to a BTreeMap model; after commit the
/// full store must match the model exactly. Deterministic op stream.
#[test]
fn kv_contract_matches_model() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();

    // A deterministic mixed workload: puts, overwrites, deletes, a range
    // delete, across three commits.
    for round in 0u32..3 {
        let mut tx = db.write_tx().unwrap();
        for i in 0..40u32 {
            let n = (i * 7 + round * 13) % 50;
            let k = format!("k{n:03}").into_bytes();
            if n % 5 == round % 5 {
                tx.del(&k).unwrap();
                model.remove(&k);
            } else {
                let v = format!("v{round}-{n}").into_bytes();
                tx.put(&k, &v).unwrap();
                model.insert(k, v);
            }
        }
        if round == 2 {
            tx.del_range(b"k010", b"k020").unwrap();
            let doomed: Vec<_> = model
                .range(b"k010".to_vec()..b"k020".to_vec())
                .map(|(k, _)| k.clone())
                .collect();
            for k in doomed {
                model.remove(&k);
            }
        }
        tx.commit().unwrap();
    }

    let tx = db.read_tx().unwrap();
    let got: Vec<_> = tx.total_scan().map(|r| r.unwrap()).collect();
    let want: Vec<_> = model
        .iter()
        .map(|(k, v)| (Slice::from(k), Slice::from(v)))
        .collect();
    assert_eq!(got, want, "store diverged from the model oracle");

    // Spot-check bounded scans against the model too.
    let got: Vec<_> = tx
        .range_scan(b"k005", b"k030")
        .map(|r| r.unwrap())
        .collect();
    let want: Vec<_> = model
        .range(b"k005".to_vec()..b"k030".to_vec())
        .map(|(k, v)| (Slice::from(k), Slice::from(v)))
        .collect();
    assert_eq!(got, want);
    assert_eq!(tx.range_count(b"k005", b"k030").unwrap(), want.len());
}

// ---------- MVCC scenarios ----------

#[test]
fn mvcc_conflict_and_discard() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    {
        let mut tx = db.write_tx().unwrap();
        tx.put(b"counter", b"0").unwrap();
        tx.commit().unwrap();
    }
    let mut tx1 = db.write_tx().unwrap();
    let mut tx2 = db.write_tx().unwrap();
    assert_eq!(tx1.get(b"counter").unwrap(), Some(Slice::from(b"0")));
    assert_eq!(tx2.get(b"counter").unwrap(), Some(Slice::from(b"0")));
    tx1.put(b"counter", b"1").unwrap();
    tx2.put(b"counter", b"2").unwrap();
    tx1.commit().unwrap();
    assert!(
        tx2.commit().is_err(),
        "second writer read a concurrently-modified key and must abort"
    );
    let tx = db.read_tx().unwrap();
    assert_eq!(
        tx.get(b"counter").unwrap(),
        Some(Slice::from(b"1")),
        "aborted transaction must leave no trace"
    );
}

#[test]
fn read_your_own_writes_and_snapshot_isolation() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let reader_before = db.read_tx().unwrap();

    let mut w = db.write_tx().unwrap();
    w.put(b"x", b"1").unwrap();
    assert_eq!(w.get(b"x").unwrap(), Some(Slice::from(b"1")), "RYOW");
    assert!(w.exists(b"x").unwrap());
    w.commit().unwrap();

    // A snapshot opened before the write never sees it.
    assert_eq!(reader_before.get(b"x").unwrap(), None, "snapshot isolation");
    // A snapshot opened after does.
    let reader_after = db.read_tx().unwrap();
    assert_eq!(reader_after.get(b"x").unwrap(), Some(Slice::from(b"1")));
}

#[test]
fn del_range_kills_own_writes_too() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    {
        let mut tx = db.write_tx().unwrap();
        tx.put(b"k1", b"1").unwrap();
        tx.put(b"k2", b"2").unwrap();
        tx.commit().unwrap();
    }
    let mut tx = db.write_tx().unwrap();
    tx.put(b"k3", b"3").unwrap();
    tx.put(b"z-outside", b"stays").unwrap();
    tx.del_range(b"k0", b"k9").unwrap();
    tx.commit().unwrap();

    let tx = db.read_tx().unwrap();
    assert_eq!(tx.get(b"k1").unwrap(), None);
    assert_eq!(tx.get(b"k2").unwrap(), None);
    assert_eq!(tx.get(b"k3").unwrap(), None, "own writes in range die too");
    assert_eq!(tx.get(b"z-outside").unwrap(), Some(Slice::from(b"stays")));
}

// ---------- time travel: seek-based scan vs naive oracle ----------

/// A bitemporal key: `[name, valid(ts), sys(sys_ts)]`, slot flags pinned
/// (the row's polarity lives in the value — see [`pol_val`]).
fn bitemp_key(rel: RelationId, name: &str, ts: i64, sys_ts: i64) -> StorageKey {
    let slot = |t: i64| DataValue::Validity(Validity::from_stored(ValidityTs::from_raw(t), true));
    let tuple: Tuple = Tuple::from_vec(vec![DataValue::Str(name.into()), slot(ts), slot(sys_ts)]);
    tuple.encode_as_key(rel)
}

/// A single-axis-shaped history row in the bitemporal format: valid
/// instant `ts` recorded once (sys = 1), asserting or retracting per the
/// flag. With one system version per instant, current-belief resolution
/// (`AsOf::current`) equals the single-axis rule, so [`as_of_oracle`]
/// stays the exact reference.
fn vld_row(rel: RelationId, name: &str, ts: i64, assert: bool) -> (StorageKey, Vec<u8>) {
    (bitemp_key(rel, name, ts, 1), pol_val(rel, assert))
}

/// A bitemporal value: the polarity byte, no payload.
fn pol_val(_rel: RelationId, assert: bool) -> Vec<u8> {
    let mut v = Vec::new();
    v.push(
        if assert {
            ClaimPolarity::Assert
        } else {
            ClaimPolarity::Retract
        }
        .encode(),
    );
    v
}

/// The naive reference: full-scan every version, group by payload, pick the
/// newest version at-or-before `at`, keep it only if assertive. Slow and
/// obviously correct — the seek-based implementation must match it exactly.
fn as_of_oracle(history: &[(&str, i64, bool)], at: i64) -> Vec<(String, i64)> {
    let mut newest: BTreeMap<String, (i64, bool)> = BTreeMap::new();
    for (name, ts, assert) in history {
        if *ts <= at {
            let e = newest.entry(name.to_string()).or_insert((*ts, *assert));
            if *ts > e.0 {
                *e = (*ts, *assert);
            }
        }
    }
    newest
        .into_iter()
        .filter(|(_, (_, assert))| *assert)
        .map(|(name, (ts, _))| (name, ts))
        .collect()
}

#[test]
fn time_travel_matches_naive_oracle() {
    // History with re-assertions, retractions, same-timestamp neighbors, and
    // an entry whose whole history is in the future.
    let history: &[(&str, i64, bool)] = &[
        ("a", 1, true),
        ("a", 3, true),
        ("a", 5, false),
        ("a", 7, true),
        ("b", 2, true),
        ("b", 6, false),
        ("c", 4, false),
        ("d", 9, true),
        ("e", 1, true),
        ("e", 2, false),
        ("e", 3, true),
        ("e", 4, false),
    ];
    let rel = RelationId::new(7).expect("below cap");
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let mut tx = db.write_tx().unwrap();
    for (name, ts, assert) in history {
        let (k, v) = vld_row(rel, name, *ts, *assert);
        tx.put(&k, &v).unwrap();
    }
    tx.commit().unwrap();

    let lower = rel.raw_encode().to_vec();
    let upper = rel.next().expect("below cap").raw_encode().to_vec();
    let tx = db.read_tx().unwrap();
    for at in 0..=10i64 {
        let got: Vec<(String, i64)> = tx
            .range_skip_scan_tuple(&lower, &upper, AsOf::current(ValidityTs::from_raw(at)))
            .map(|r| {
                let t = r.unwrap();
                let name = match &t.as_slice()[0] {
                    DataValue::Str(s) => s.to_string(),
                    v @ (data_value_any!()) => panic!("unexpected {v:?}"),
                };
                let ts = match &t.as_slice()[1] {
                    DataValue::Validity(v) => v.ts_micros(),
                    v @ (data_value_any!()) => panic!("unexpected {v:?}"),
                };
                (name, ts)
            })
            .collect();
        let want = as_of_oracle(history, at);
        assert_eq!(
            got, want,
            "as-of {at}: seek scan diverged from the naive oracle"
        );
    }
}

#[test]
fn time_travel_sees_own_writes() {
    let rel = RelationId::new(7).expect("below cap");
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    {
        let mut tx = db.write_tx().unwrap();
        let (k, v) = vld_row(rel, "a", 1, true);
        tx.put(&k, &v).unwrap();
        tx.commit().unwrap();
    }
    let mut tx = db.write_tx().unwrap();
    let (k, v) = vld_row(rel, "b", 2, true);
    tx.put(&k, &v).unwrap();
    let lower = rel.raw_encode().to_vec();
    let upper = rel.next().expect("below cap").raw_encode().to_vec();
    let got: Vec<String> = tx
        .range_skip_scan_tuple(&lower, &upper, AsOf::current(ValidityTs::from_raw(5)))
        .map(|r| match &r.unwrap()[0] {
            DataValue::Str(s) => s.to_string(),
            v @ (data_value_any!()) => panic!("unexpected {v:?}"),
        })
        .collect();
    assert_eq!(got, vec!["a".to_string(), "b".to_string()]);
    tx.commit().unwrap();
}

// ---------- backup ----------

#[test]
fn backup_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let src = new_fjall_storage(dir.path().join("src")).unwrap();
    let mut tx = src.write_tx().unwrap();
    for i in 0..100u32 {
        tx.put(
            format!("key-{i:04}").as_bytes(),
            format!("val-{i}").as_bytes(),
        )
        .unwrap();
    }
    tx.commit().unwrap();

    let dump = dir.path().join("dump.kyzo");
    dump_storage(&src, &dump).unwrap();
    let dst = new_fjall_storage(dir.path().join("dst")).unwrap();
    restore_storage(&dst, &dump).unwrap();

    let ta = src.read_tx().unwrap();
    let a: Vec<_> = ta.total_scan().map(|r| r.unwrap()).collect();
    let tb = dst.read_tx().unwrap();
    let b: Vec<_> = tb.total_scan().map(|r| r.unwrap()).collect();
    assert_eq!(a.len(), 100);
    assert_eq!(a, b, "restored store must equal the source");
}

/// The lightest-weight cataloged relation the dump backstop can classify:
/// zero-arity, no triggers/indices/constraints, `keyspace_kind: Facts`. The
/// backstop only ever reads `id` and `keyspace_kind` off a catalog row, so
/// every other field is filler.
fn facts_handle(id: RelationId, name: &str) -> RelationHandle {
    RelationHandle {
        name: SmartString::<LazyCompact>::from(name),
        id,
        metadata: StoredRelationMetadata {
            keys: vec![],
            non_keys: vec![],
        },
        put_triggers: vec![],
        rm_triggers: vec![],
        replace_triggers: vec![],
        access_level: AccessLevel::default(),
        indices: vec![],
        description: SmartString::default(),
        constraints: vec![],
        keyspace_kind: KeyspaceKind::Facts,
    }
}

/// A hand-built bitemporal fact key with a CHOSEN system stamp — bypassing
/// `tx.system_stamp()` entirely, exactly like [`bitemp_key`], so a test can
/// mint a stamp the real clock would never produce.
fn stamped_row(
    rel: RelationId,
    name: &str,
    valid_ts: i64,
    sys: ValidityTs,
) -> (StorageKey, Vec<u8>) {
    let slot = |ts: ValidityTs| DataValue::Validity(Validity::from_stored(ts, true));
    let tuple: Tuple = Tuple::from_vec(vec![
        DataValue::Str(name.into()),
        slot(ValidityTs::from_raw(valid_ts)),
        slot(sys),
    ]);
    (tuple.encode_as_key(rel), pol_val(rel, true))
}

/// Sabotage-verify the dump backstop (layer 3 of the clock-floor fix,
/// `storage/backup.rs`): a `Facts` row whose stored system stamp exceeds
/// the store's own clock floor is exactly the corruption class the
/// historical race could silently produce (see the module's contract
/// history). Confirm the backstop refuses it with a TYPED error rather
/// than silently writing a lying dump.
#[test]
fn dump_refuses_a_row_stamped_above_its_own_floor() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let rel = RelationId::new(100).expect("below cap");
    let handle = facts_handle(rel, "floor_test");

    // Far enough in the future that it exceeds any floor a store opened
    // "now" could possibly report (mirrors the existing
    // `restore_raises_clock_floor_past_imported_stamps` convention).
    let bad_sys =
        ValidityTs::from_raw(crate::runtime::current_validity().unwrap().raw() + 1_000_000_000);
    let (key, val) = stamped_row(rel, "evil", 1, bad_sys);

    let mut tx = db.write_tx().unwrap();
    tx.put(
        &SystemKey::Relation("floor_test").encode(),
        &handle.encode().unwrap(),
    )
    .unwrap();
    tx.put(&key, &val).unwrap();
    tx.commit().unwrap();

    let dump = dir.path().join("dump.kyzo");
    let err = dump_storage(&db, &dump).unwrap_err();
    assert!(
        err.downcast_ref::<DumpClockFloorViolation>().is_some(),
        "expected a typed DumpClockFloorViolation, got: {err}"
    );

    // Sanity: reverting to the historical (broken) order — floor read
    // BEFORE the snapshot — would have raced with nothing here (this is a
    // single-threaded sabotage, not the concurrency pin below), but the
    // backstop must fire independently of timing since the bad row is
    // already committed before the floor is ever read. Confirm a row
    // whose stamp legitimately sits at-or-below the floor is unaffected.
    let dir2 = tempfile::tempdir().unwrap();
    let db2 = new_fjall_storage(dir2.path()).unwrap();
    let handle2 = facts_handle(rel, "floor_test");
    let mut tx2 = db2.write_tx().unwrap();
    let ok_sys = tx2.system_stamp();
    let (key2, val2) = stamped_row(rel, "fine", 1, ok_sys);
    tx2.put(
        &SystemKey::Relation("floor_test").encode(),
        &handle2.encode().unwrap(),
    )
    .unwrap();
    tx2.put(&key2, &val2).unwrap();
    tx2.commit().unwrap();
    let dump2 = dir2.path().join("dump.kyzo");
    dump_storage(&db2, &dump2).unwrap();
}

/// Concurrency pin for the dump clock-floor fix: real writer threads mint
/// real stamps through the storage clock (`tx.system_stamp()`) while dumps
/// run in a loop on the main thread. For every dump produced, INDEPENDENTLY
/// re-parse the dump FILE's bytes (not the in-process values `dump_storage`
/// itself computed) and confirm every `Facts` row's system stamp is `<=`
/// that same dump's own recorded floor — the exact property the historical
/// race broke. With the fix this must hold on every cycle; the race window
/// this closes was narrow, so many cycles under real contention are what
/// would have caught it occasionally before the fix.
#[test]
fn dumps_never_advertise_a_floor_below_their_own_rows_under_concurrent_writers() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let rel = RelationId::new(42).expect("below cap");
    let handle = facts_handle(rel, "floor_race");
    {
        let mut tx = db.write_tx().unwrap();
        tx.put(
            &SystemKey::Relation("floor_race").encode(),
            &handle.encode().unwrap(),
        )
        .unwrap();
        tx.commit().unwrap();
    }

    const WRITERS: usize = 8;
    const CYCLES: usize = 200;
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    let writers: Vec<_> = (0..WRITERS)
        .map(|w| {
            let db = db.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut i: u64 = 0;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let mut tx = db.write_tx().unwrap();
                    let sys = tx.system_stamp();
                    let (k, v) = stamped_row(rel, &format!("w{w}-{i}"), 1, sys);
                    tx.put(&k, &v).unwrap();
                    tx.commit().unwrap();
                    i += 1;
                }
            })
        })
        .collect();

    let dump_path = dir.path().join("race.kyzo");
    let rel_prefix = rel.raw_encode();

    // Wait for the writers to actually get going before timing dump
    // cycles: under heavy parallel test-binary contention, thread
    // spawn/schedule delay can otherwise let the very first dump land
    // before any writer has committed a row, which is a scheduling
    // artifact unrelated to the property under test. Rows are never
    // deleted here, so once one appears the store only grows from here.
    let upper = rel.next().expect("below cap").raw_encode();
    while db
        .read_tx()
        .unwrap()
        .range_count(&rel_prefix, &upper)
        .unwrap()
        == 0
    {
        std::thread::yield_now();
    }

    for cycle in 0..CYCLES {
        dump_storage(&db, &dump_path).unwrap();

        let bytes = std::fs::read(&dump_path).unwrap();
        assert_eq!(&bytes[0..8], b"KYZODMP2".as_slice());
        let mut off = 8usize;
        let version_len = u64::from_be_bytes(bytes[off..off + 8].try_into().unwrap()) as usize;
        off += 8 + version_len;
        let floor = i64::from_be_bytes(bytes[off..off + 8].try_into().unwrap());
        off += 8;

        let mut checked = 0u64;
        while off < bytes.len() {
            let klen = u64::from_be_bytes(bytes[off..off + 8].try_into().unwrap()) as usize;
            off += 8;
            let key = &bytes[off..off + klen];
            off += klen;
            let vlen = u64::from_be_bytes(bytes[off..off + 8].try_into().unwrap()) as usize;
            off += 8 + vlen;
            if key.len() >= 8 && key[0..8] == rel_prefix {
                let stamp = crate::data::bitemporal::system_stamp_of_key(key).unwrap();
                assert!(
                    stamp.raw() <= floor,
                    "dump cycle {cycle}: row stamped {} exceeds this dump's own floor {floor}",
                    stamp.raw()
                );
                checked += 1;
            }
        }
        assert!(checked > 0, "dump cycle {cycle} saw no fact rows to check");
    }

    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    for w in writers {
        w.join().unwrap();
    }
}

// ---------- sentinel and corruption edge cases ----------

/// A stored retraction at ts == i64::MIN in BOTH slots sits as close to
/// the TERMINAL_VALIDITY seek sentinel as a storable key can (the
/// sentinel itself carries a retract flag, which no longer parses); the
/// scan must terminate (and skip it) rather than livelock.
#[test]
fn skip_scan_terminates_on_retraction_at_min_ts() {
    let rel = RelationId::new(7).expect("below cap");
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let mut tx = db.write_tx().unwrap();
    let (k, v) = vld_row(rel, "a", 1, true);
    tx.put(&k, &v).unwrap();
    tx.put(
        &bitemp_key(rel, "z", i64::MIN, i64::MIN),
        &pol_val(rel, false),
    )
    .unwrap();
    tx.commit().unwrap();

    let tx = db.read_tx().unwrap();
    let got: Vec<String> = tx
        .range_skip_scan_tuple(
            rel.raw_encode().as_ref(),
            rel.next().expect("below cap").raw_encode().as_ref(),
            AsOf::current(ValidityTs::from_raw(5)),
        )
        .map(|r| match &r.unwrap()[0] {
            DataValue::Str(s) => s.to_string(),
            v @ (data_value_any!()) => panic!("unexpected {v:?}"),
        })
        .collect();
    assert_eq!(got, vec!["a".to_string()]);
}

/// An assertion at ts == i64::MIN is a legitimate hit and must also terminate.
#[test]
fn skip_scan_hit_at_min_ts_terminates() {
    let rel = RelationId::new(7).expect("below cap");
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let mut tx = db.write_tx().unwrap();
    tx.put(
        &bitemp_key(rel, "a", i64::MIN, i64::MIN),
        &pol_val(rel, true),
    )
    .unwrap();
    tx.commit().unwrap();

    let tx = db.read_tx().unwrap();
    let got: Vec<Tuple> = tx
        .range_skip_scan_tuple(
            rel.raw_encode().as_ref(),
            rel.next().expect("below cap").raw_encode().as_ref(),
            AsOf::current(ValidityTs::from_raw(0)),
        )
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(got.len(), 1);
}

/// Corruption classes that could otherwise panic or abort the process must
/// all be errors: out-of-range relation ids, overflowing vector lengths,
/// over-deep nesting.
#[test]
fn corrupt_inputs_error_never_panic() {
    // Relation id beyond 2^48: the skip path treats the prefix as opaque
    // bytes, so the requirement is simply no panic, ever.
    let mut k = vec![0xFF; 8];
    let mut enc = vec![];
    crate::data::value::append_canonical(
        &mut enc,
        &DataValue::Validity(Validity::new(ValidityTs::from_raw(1), true).expect("non-reserved")),
    );
    k.extend(enc);
    let _ = crate::data::bitemporal::check_key_for_bitemporal(
        &k,
        crate::data::bitemporal::ClaimPolarity::Assert,
        AsOf::current(ValidityTs::from_raw(5)),
        None,
    );

    // A vector length prefix whose byte size overflows usize multiplication.
    let mut k = vec![0x0B, 0x01];
    k.extend([0xFF; 8]);
    assert!(DataValue::decode_from_key(&k).is_err());

    // 200k nested list tags: recursion must be depth-bounded.
    let k = vec![0x09; 200_000];
    assert!(DataValue::decode_from_key(&k).is_err());
}

/// Deterministic corruption harness: every single-byte mutation of every
/// corpus encoding must decode to an error or a value — never a panic. This
/// covers the structured-corruption space the random Law 3 generator misses.
#[test]
fn law3_byte_flip_harness() {
    for v in corpus() {
        let buf = encode(&v);
        for i in 0..buf.len() {
            for flip in [0x01u8, 0x80, 0xFF] {
                let mut m = buf.clone();
                m[i] ^= flip;
                let _ = DataValue::decode_from_key(&m);
            }
        }
    }
}

/// A fresh store is stamped with format v1 and reopens cleanly; a store
/// stamped with a different version must refuse to open.
#[test]
fn format_version_stamp_and_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    {
        let db = new_fjall_storage(&path).unwrap();
        let mut tx = db.write_tx().unwrap();
        tx.put(b"k", b"v").unwrap();
        tx.commit().unwrap();
    }
    // Reopen: same version, must succeed and see the data.
    {
        let db = new_fjall_storage(&path).unwrap();
        let tx = db.read_tx().unwrap();
        assert_eq!(tx.get(b"k").unwrap(), Some(Slice::from(b"v")));
    }
    // Tamper the version stamp, reopen must fail loudly.
    {
        let raw = fjall::OptimisticTxDatabase::builder(&path).open().unwrap();
        let meta = raw
            .keyspace("kyzo_meta", fjall::KeyspaceCreateOptions::default)
            .unwrap();
        meta.insert(b"format_version", b"99").unwrap();
    }
    assert!(
        new_fjall_storage(&path).is_err(),
        "version mismatch must refuse to open"
    );
}

/// The #119 migration boundary, made explicit and executable. The value
/// plane changed the on-disk VALUE format (canonical bytes replace the old
/// self-describing payload), so `FormatVersion::CURRENT` is 5. Per the
/// prime directive there are no deployed stores and no in-place migration:
/// a store written by any PRE-#119 build (version 4) must simply REFUSE to
/// open, never be silently misread by the new decoder. This pins that the
/// previous version is the one refused (not just some arbitrary tamper),
/// and that CURRENT parses as exactly 5.
#[test]
fn pre_value_plane_stores_v4_refuse_to_open() {
    use crate::storage::FormatVersion;
    // CURRENT is 5, and 5 is what a fresh store stamps.
    assert_eq!(FormatVersion::CURRENT, FormatVersion::parse(b"5").unwrap());
    // The immediately-previous format (4) is a DIFFERENT version and so is
    // refused at the door — the value format is incompatible.
    let v4 = FormatVersion::parse(b"4").unwrap();
    assert_ne!(
        v4,
        FormatVersion::CURRENT,
        "v4 must not equal the current format"
    );

    // End to end: a store carrying the v4 stamp refuses to open.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    {
        let db = new_fjall_storage(&path).unwrap();
        let mut tx = db.write_tx().unwrap();
        tx.put(b"k", b"v").unwrap();
        tx.commit().unwrap();
    }
    {
        let raw = fjall::OptimisticTxDatabase::builder(&path).open().unwrap();
        let meta = raw
            .keyspace("kyzo_meta", fjall::KeyspaceCreateOptions::default)
            .unwrap();
        meta.insert(b"format_version", b"4").unwrap();
    }
    assert!(
        new_fjall_storage(&path).is_err(),
        "a pre-#119 (v4) store must refuse to open, never be misread by the v5 decoder"
    );
}

// ---------- crash consistency ----------

/// Process-crash consistency: a child process commits one transaction,
/// stages a second without committing, then `abort()`s. Reopening the store
/// must show every committed write and nothing from the uncommitted one.
///
/// Scope stated honestly: `abort()` simulates a process crash (committed
/// data has reached OS buffers — fjall's `Buffer` persist mode). A power
/// cut is a stronger event; surviving it is what `Storage::sync` (fsync)
/// is for, and testing THAT honestly requires fault-injection infrastructure
/// (e.g. dm-flakey), not a unit test that lies about what it simulates.
#[test]
fn crash_consistency_process_abort() {
    if let Ok(dir) = std::env::var("KYZO_CRASH_CHILD_DIR") {
        // ---- child: do the work, then die without cleanup ----
        let db = new_fjall_storage(&dir).unwrap();
        // First key: committed WITHOUT sync — tests the Buffer durability
        // claim itself (commit alone must survive a process crash).
        let mut tx = db.write_tx().unwrap();
        tx.put(b"committed", b"survives").unwrap();
        tx.commit().unwrap();
        // Second key: committed and fsynced.
        let mut tx = db.write_tx().unwrap();
        tx.put(b"synced", b"survives-power-cut-too").unwrap();
        tx.commit().unwrap();
        db.sync().unwrap();
        // Third key: the per-transaction durable commit.
        let mut tx = db.write_tx().unwrap();
        tx.put(b"durable", b"per-tx-fsync").unwrap();
        tx.commit_durable().unwrap();
        let mut tx = db.write_tx().unwrap();
        tx.put(b"uncommitted", b"must-vanish").unwrap();
        // No commit. Die hard: no destructors, no flushes.
        std::process::abort();
    }

    // ---- parent ----
    let dir = tempfile::tempdir().unwrap();
    let exe = std::env::current_exe().unwrap();
    let status = std::process::Command::new(exe)
        .args([
            "storage::tests::crash_consistency_process_abort",
            "--exact",
            "--nocapture",
        ])
        .env("KYZO_CRASH_CHILD_DIR", dir.path().join("db"))
        .status()
        .unwrap();
    assert!(
        !status.success(),
        "child must die by abort, not exit cleanly"
    );

    let db = new_fjall_storage(dir.path().join("db")).unwrap();
    let tx = db.read_tx().unwrap();
    assert_eq!(
        tx.get(b"committed").unwrap(),
        Some(Slice::from(b"survives")),
        "committed (unsynced) data must survive a process crash"
    );
    assert_eq!(
        tx.get(b"synced").unwrap(),
        Some(Slice::from(b"survives-power-cut-too")),
        "synced data must survive a process crash"
    );
    assert_eq!(
        tx.get(b"durable").unwrap(),
        Some(Slice::from(b"per-tx-fsync")),
        "commit_durable data must survive a process crash"
    );
    assert_eq!(
        tx.get(b"uncommitted").unwrap(),
        None,
        "uncommitted data must vanish in a process crash"
    );
}

/// batch_put applies atomic chunks; restore demands an empty target, so an
/// interrupted restore is always recoverable by discard-and-rerun.
#[test]
fn restore_refuses_nonempty_target() {
    let dir = tempfile::tempdir().unwrap();
    let src = new_fjall_storage(dir.path().join("src")).unwrap();
    let mut tx = src.write_tx().unwrap();
    tx.put(b"a", b"1").unwrap();
    tx.commit().unwrap();
    let dump = dir.path().join("d.kyzo");
    dump_storage(&src, &dump).unwrap();

    let dst = new_fjall_storage(dir.path().join("dst")).unwrap();
    let mut tx = dst.write_tx().unwrap();
    tx.put(b"existing", b"data").unwrap();
    tx.commit().unwrap();
    assert!(
        restore_storage(&dst, &dump).is_err(),
        "restore into a non-empty store must be refused"
    );
    // A fresh store takes it fine.
    let fresh = new_fjall_storage(dir.path().join("fresh")).unwrap();
    restore_storage(&fresh, &dump).unwrap();
    let tx = fresh.read_tx().unwrap();
    assert_eq!(tx.get(b"a").unwrap(), Some(Slice::from(b"1")));
}

/// batch_put crossing its chunk boundary: an import larger than one chunk
/// lands complete and correct.
#[test]
fn batch_put_crosses_chunk_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let n: u32 = 40_000; // > one 32_768 chunk
    let iter = (0..n).map(|i| {
        Ok((
            format!("k{i:08}").into_bytes(),
            format!("v{i}").into_bytes(),
        ))
    });
    db.batch_put(Box::new(iter)).unwrap();
    let tx = db.read_tx().unwrap();
    assert_eq!(tx.range_count(b"k", b"l").unwrap(), n as usize);
    assert_eq!(tx.get(b"k00039999").unwrap(), Some(Slice::from(b"v39999")));
}

// ---------- true concurrency ----------

/// Concurrent writers are a core requirement (the reason fjall was chosen):
/// real threads, genuinely parallel write transactions.
///
/// Disjoint keys: every writer must succeed with zero conflicts.
/// Contended key: writers race with a read-modify-write and retry on
/// conflict; the final value must equal the total number of increments —
/// proving conflicts are detected (no lost updates) and progress is made
/// (no livelock/deadlock).
#[test]
fn concurrent_writers_across_threads() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    {
        let mut tx = db.write_tx().unwrap();
        tx.put(b"counter", b"0").unwrap();
        tx.commit().unwrap();
    }

    const THREADS: usize = 8;
    const OPS: usize = 25;
    std::thread::scope(|s| {
        for t in 0..THREADS {
            let db = db.clone();
            s.spawn(move || {
                // Disjoint writes: must never conflict.
                for i in 0..OPS {
                    let mut tx = db.write_tx().unwrap();
                    tx.put(format!("t{t}-k{i}").as_bytes(), b"x").unwrap();
                    tx.commit()
                        .unwrap_or_else(|e| panic!("disjoint writers must not conflict: {e}"));
                }
                // Contended increments: retry on conflict until applied.
                for _ in 0..OPS {
                    loop {
                        let mut tx = db.write_tx().unwrap();
                        let cur: u64 = std::str::from_utf8(&tx.get(b"counter").unwrap().unwrap())
                            .unwrap()
                            .parse()
                            .unwrap();
                        tx.put(b"counter", (cur + 1).to_string().as_bytes())
                            .unwrap();
                        if tx.commit().is_ok() {
                            break;
                        }
                    }
                }
            });
        }
    });

    let tx = db.read_tx().unwrap();
    let total: u64 = std::str::from_utf8(&tx.get(b"counter").unwrap().unwrap())
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        total,
        (THREADS * OPS) as u64,
        "every increment must land exactly once: conflicts detected, no lost updates"
    );
    assert_eq!(
        tx.range_count(b"t", b"u").unwrap(),
        THREADS * OPS,
        "all disjoint writes must be present"
    );
}

/// Compile-time contract: transactions move across threads (Send) and are
/// shared by reference across threads (Sync — the engine's parallel query
/// evaluation depends on it). Storage handles are Send + Sync + Clone.
#[test]
fn concurrency_bounds_are_compiler_checked() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<crate::storage::fjall::FjallStorage>();
    assert_send_sync::<crate::storage::fjall::FjallReadTx>();
    assert_send_sync::<crate::storage::fjall::FjallWriteTx>();
}

// ---------- value-side decoding laws ----------

/// Law 3, value side: stored VALUE payloads are decoded with rmp-serde; a
/// corrupt or hostile payload must be an error, never a panic. The named
/// payload reaches the RegexWrapper deserializer in 14 bytes.
#[test]
fn law3_value_payloads_error_never_panic() {
    use crate::data::value::extend_tuple_from_v;
    // rmp payload: array[1] { map{ "Regex": "" } } behind the 8-byte header.
    let mut hostile = vec![0u8; 8];
    hostile.extend([0x91, 0x81, 0xa5]);
    hostile.extend(b"Regex");
    hostile.push(0xa0);
    let mut tup: Tuple = Tuple::new();
    assert!(extend_tuple_from_v(&mut tup, &hostile).is_err());
}

proptest! {
    /// Generative value-side Law 3: arbitrary value bytes never panic.
    #[test]
    fn law3_value_generative(bytes in proptest::collection::vec(any::<u8>(), 0..64)) {
        let mut tup: Tuple = Tuple::new();
        let _ = crate::data::value::extend_tuple_from_v(&mut tup, &bytes);
    }
}

/// del_range around its chunk boundary (CHUNK = 1024): the resuming-cursor
/// loop must be exact at 1023/1024/1025/2048.
#[test]
fn del_range_chunk_boundaries() {
    for n in [1023usize, 1024, 1025, 2048] {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let iter = (0..n).map(|i| Ok((format!("k{i:08}").into_bytes(), b"v".to_vec())));
        db.batch_put(Box::new(iter)).unwrap();
        let mut tx = db.write_tx().unwrap();
        tx.put(b"z-survivor", b"x").unwrap();
        tx.del_range(b"k", b"l").unwrap();
        tx.commit().unwrap();
        let tx = db.read_tx().unwrap();
        assert_eq!(tx.range_count(b"k", b"l").unwrap(), 0, "n={n}: all deleted");
        assert!(
            tx.exists(b"z-survivor").unwrap(),
            "n={n}: outside range survives"
        );
    }
}

/// Phantom protection: a range READ in a write transaction is conflict-
/// tracked — inserting into that range concurrently must abort the reader's
/// commit even though it wrote elsewhere.
#[test]
fn range_reads_get_phantom_protection() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let mut tx1 = db.write_tx().unwrap();
    let seen: usize = tx1.range_scan(b"r", b"s").count();
    assert_eq!(seen, 0);
    let mut tx2 = db.write_tx().unwrap();
    tx2.put(b"r-phantom", b"x").unwrap();
    tx2.commit().unwrap();
    tx1.put(b"elsewhere", b"y").unwrap();
    assert!(
        tx1.commit().is_err(),
        "a scanned range was modified concurrently: SSI must abort the scanner"
    );
}

/// A live iterator holds its snapshot: a commit landing mid-iteration must
/// not change what the iterator sees.
#[test]
fn live_iterator_is_snapshot_stable() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let iter = (0..100u32).map(|i| Ok((format!("k{i:04}").into_bytes(), b"v".to_vec())));
    db.batch_put(Box::new(iter)).unwrap();

    let reader = db.read_tx().unwrap();
    let mut scan = reader.total_scan();
    let mut count = 0;
    for _ in 0..50 {
        scan.next().unwrap().unwrap();
        count += 1;
    }
    // A writer lands 100 more keys mid-scan.
    let mut w = db.write_tx().unwrap();
    for i in 100..200u32 {
        w.put(format!("k{i:04}").as_bytes(), b"v").unwrap();
    }
    w.commit().unwrap();
    for item in scan {
        item.unwrap();
        count += 1;
    }
    assert_eq!(
        count, 100,
        "iterator must see exactly its snapshot, not the mid-scan commit"
    );
}

/// Backup edges: empty store round-trips; truncated and huge-length dumps
/// are errors, never aborts.
#[test]
fn backup_edge_cases() {
    let dir = tempfile::tempdir().unwrap();
    // Empty store round-trip.
    let empty = new_fjall_storage(dir.path().join("empty")).unwrap();
    let dump = dir.path().join("empty.kyzo");
    dump_storage(&empty, &dump).unwrap();
    let fresh = new_fjall_storage(dir.path().join("fresh")).unwrap();
    restore_storage(&fresh, &dump).unwrap();
    let tx = fresh.read_tx().unwrap();
    assert!(tx.total_scan().next().is_none());

    // Huge length prefix: error, not an allocation abort.
    let evil = dir.path().join("evil.kyzo");
    let mut bytes = b"KYZODMP2".to_vec();
    bytes.extend((1u64).to_be_bytes());
    bytes.extend(b"1");
    bytes.extend(u64::MAX.to_be_bytes());
    std::fs::write(&evil, &bytes).unwrap();
    let victim = new_fjall_storage(dir.path().join("victim")).unwrap();
    assert!(restore_storage(&victim, &evil).is_err());

    // Truncated mid-pair: error.
    let cut = dir.path().join("cut.kyzo");
    let mut bytes = b"KYZODMP2".to_vec();
    bytes.extend((1u64).to_be_bytes());
    bytes.extend(b"1");
    bytes.extend((3u64).to_be_bytes());
    bytes.extend(b"key");
    std::fs::write(&cut, &bytes).unwrap();
    let victim2 = new_fjall_storage(dir.path().join("victim2")).unwrap();
    assert!(restore_storage(&victim2, &cut).is_err());
}

/// Degenerate ranges are contract behavior, pinned: inverted and empty
/// ranges yield nothing and never panic.
#[test]
fn degenerate_ranges_pinned() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let mut tx = db.write_tx().unwrap();
    tx.put(b"m", b"1").unwrap();
    assert_eq!(
        tx.range_scan(b"z", b"a").count(),
        0,
        "inverted range is empty"
    );
    assert_eq!(tx.range_scan(b"m", b"m").count(), 0, "empty range is empty");
    tx.del_range(b"z", b"a").unwrap();
    tx.commit().unwrap();
    let tx = db.read_tx().unwrap();
    assert!(tx.exists(b"m").unwrap());
}

/// Regression for the store-poisoning kernel bug: fjall records requested
/// range bounds VERBATIM in a write transaction's conflict manager and
/// replays them through `BTreeSet::range` at COMMIT time — which panics on
/// an inverted range, inside the commit oracle, while holding the global
/// write-serialize lock, poisoning the whole store. The panic shape needs
/// contention (validation only runs against post-snapshot commits), so this
/// test tracks inverted ranges through every range entry point in a live
/// write transaction, lands a concurrent committed write, then commits: the
/// result must be a clean empty scan + a successful commit, and the store
/// must stay fully usable afterward (no poisoned lock).
#[test]
fn inverted_ranges_under_contention_commit_clean() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    {
        let mut tx = db.write_tx().unwrap();
        tx.put(b"a", b"1").unwrap();
        tx.put(b"m", b"2").unwrap();
        tx.commit().unwrap();
    }
    let mut tx = db.write_tx().unwrap();
    // Every range entry point, with inverted (and empty) bounds: all must
    // be empty results, and none may reach fjall's read tracking.
    assert_eq!(tx.range_scan(b"z", b"a").count(), 0, "inverted scan");
    assert_eq!(tx.range_scan(b"m", b"m").count(), 0, "empty scan");
    tx.del_range(b"z", b"a").unwrap(); // inverted del_range is a no-op
    assert_eq!(
        tx.range_skip_scan_tuple(b"z", b"a", AsOf::current(ValidityTs::from_raw(0)))
            .count(),
        0,
        "inverted skip scan"
    );
    // The contention that arms the commit-time validation.
    {
        let mut w = db.write_tx().unwrap();
        w.put(b"c", b"concurrent").unwrap();
        w.commit().unwrap();
    }
    tx.put(b"mine", b"x").unwrap();
    tx.commit()
        .expect("inverted ranges tracked nothing: commit must succeed, not panic");
    // The store stays usable: the write-serialize lock was never poisoned.
    let mut tx = db.write_tx().unwrap();
    tx.put(b"after", b"ok").unwrap();
    tx.commit().unwrap();
    let r = db.read_tx().unwrap();
    assert_eq!(
        r.get(b"m").unwrap(),
        Some(Slice::from(b"2")),
        "inverted del_range must delete nothing"
    );
    assert_eq!(r.get(b"mine").unwrap(), Some(Slice::from(b"x")));
    assert_eq!(r.get(b"after").unwrap(), Some(Slice::from(b"ok")));
}

/// The conflict surface is READS AND WRITES (contract v2), pinned on the
/// REAL backend: a blind write-write race aborts its second committer with
/// the typed [`ConflictError`] — first-committer-wins, the rerun converges —
/// while blind writes to DISJOINT keys still never conflict, and a write
/// transaction with an empty write set commits without certifying its reads
/// — even reads that were concurrently clobbered. The identical pin lives on
/// the sim in `sim_mvcc_semantics_smoke`; the two must stay together.
///
/// (Contract v1 pinned the opposite: both blind writers committed,
/// serialized last-writer-wins. Re-pinned KNOWINGLY under the story #3
/// ruling — see storage/mod.rs, "Contract history".)
#[test]
fn write_write_race_aborts_second_committer() {
    use crate::storage::ConflictError;
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    // Blind write-write on the same key: the first committer wins, the
    // second aborts with the typed conflict even though neither side read.
    let mut tx1 = db.write_tx().unwrap();
    let mut tx2 = db.write_tx().unwrap();
    tx1.put(b"ww", b"1").unwrap();
    tx2.put(b"ww", b"2").unwrap();
    tx1.commit().expect("the FIRST committer must never abort");
    let err = tx2
        .commit()
        .expect_err("a write-write race must abort the second committer");
    assert!(
        err.is_conflict(),
        "the write-write abort must be the typed, retryable conflict: {err:?}"
    );
    assert_eq!(
        db.read_tx().unwrap().get(b"ww").unwrap(),
        Some(Slice::from(b"1")),
        "the aborted writer must leave no trace: first committer wins"
    );
    // The loser's rerun (fresh snapshot, per the retry contract) converges.
    let mut retry = db.write_tx().unwrap();
    retry.put(b"ww", b"2").unwrap();
    retry
        .commit()
        .expect("the rerun on a fresh snapshot commits");
    assert_eq!(
        db.read_tx().unwrap().get(b"ww").unwrap(),
        Some(Slice::from(b"2"))
    );
    // A blind DELETE races like a blind put: writes of both species are on
    // the conflict surface.
    let mut tx1 = db.write_tx().unwrap();
    let mut tx2 = db.write_tx().unwrap();
    tx1.put(b"ww", b"3").unwrap();
    tx2.del(b"ww").unwrap();
    tx1.commit().unwrap();
    let err = tx2
        .commit()
        .expect_err("del vs put on one key is a write-write race");
    assert!(err.is_conflict());
    // Disjoint blind writers still never conflict.
    let mut tx1 = db.write_tx().unwrap();
    let mut tx2 = db.write_tx().unwrap();
    tx1.put(b"d-left", b"1").unwrap();
    tx2.put(b"d-right", b"2").unwrap();
    tx1.commit().expect("disjoint writers must not conflict");
    tx2.commit().expect("disjoint writers must not conflict");
    // Empty write set: commit returns before the oracle runs; the clobbered
    // read is never validated. A read-only WriteTx commit certifies nothing.
    let ro = db.write_tx().unwrap();
    assert_eq!(ro.get(b"ww").unwrap(), Some(Slice::from(b"3")));
    {
        let mut w = db.write_tx().unwrap();
        w.put(b"ww", b"4").unwrap();
        w.commit().unwrap();
    }
    ro.commit()
        .expect("an empty-write-set commit never aborts, clobbered reads or not");
}

/// Conflicts are a TYPED, retryable error the engine can match on — not a
/// string. Also pins that options-configured stores work and expose stats.
#[test]
fn conflict_is_typed_and_options_and_stats_work() {
    use crate::storage::ConflictError;
    use crate::storage::fjall::{StorageOptions, new_fjall_storage_with};
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage_with(
        dir.path(),
        StorageOptions {
            cache_size_bytes: Some(8 * 1024 * 1024),
            worker_threads: Some(2),
            ..Default::default()
        },
    )
    .unwrap();
    {
        let mut tx = db.write_tx().unwrap();
        tx.put(b"k", b"0").unwrap();
        tx.commit().unwrap();
    }
    let mut tx1 = db.write_tx().unwrap();
    let mut tx2 = db.write_tx().unwrap();
    let _ = tx1.get(b"k").unwrap();
    let _ = tx2.get(b"k").unwrap();
    tx1.put(b"k", b"1").unwrap();
    tx2.put(b"k", b"2").unwrap();
    tx1.commit().unwrap();
    let err = tx2.commit().unwrap_err();
    assert!(
        err.is_conflict(),
        "conflict must be matchable as ConflictError, got: {err:?}"
    );
    let stats = db.stats();
    assert!(stats.cache_capacity_bytes > 0);
}

// ---------- integrity verification ----------

/// verify_storage: clean on a healthy store; locates injected corruption
/// (planted through a raw fjall handle, below the kernel's write path) and
/// keeps walking rather than stopping at the first wound.
#[test]
fn verify_storage_reports_injected_corruption() {
    use crate::storage::verify::verify_storage;

    use crate::runtime::db::Db;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let clean_checked;
    {
        // A REAL, kernel-written store: the catalog-aware verifier needs the
        // real entry taxonomy (msgpack catalog + bitemporal data rows), so the
        // fixture is a genuine relation with genuine rows, not raw puts below
        // the kernel (which would be dangling, cataloged nowhere).
        let storage = new_fjall_storage(&path).unwrap();
        let db = Db::new(storage.clone()).unwrap();
        db.run_script(
            "?[k, v] <- [[1, 7], [2, 14], [3, 21], [4, 28], [5, 35]] :create rel {k => v}",
            std::collections::BTreeMap::new(),
        )
        .unwrap();
        let report = verify_storage(&storage).unwrap();
        assert!(
            report.is_clean(),
            "a real, healthy store must verify clean (catalog + bitemporal rows): {report:?}"
        );
        assert!(
            report.checked >= 5,
            "the five data rows are walked: {report:?}"
        );
        clean_checked = report.checked;
    }
    // Inject garbage below the kernel: a raw fjall write of a key whose column
    // bytes do not decode (0xEE is no valid tag). Verification must surface it
    // and keep walking, not stop at the first wound.
    {
        let raw = fjall::OptimisticTxDatabase::builder(&path).open().unwrap();
        let ks = raw
            .keyspace("kyzo", fjall::KeyspaceCreateOptions::default)
            .unwrap();
        ks.insert([0u8, 0, 0, 0, 0, 0, 0, 7, 0xEE, 0xEE], b"?")
            .unwrap();
        raw.persist(fjall::PersistMode::SyncAll).unwrap();
    }
    let storage = new_fjall_storage(&path).unwrap();
    let report = verify_storage(&storage).unwrap();
    assert!(!report.is_clean());
    assert_eq!(
        report.checked,
        clean_checked + 1,
        "the walk continues past the wound: {report:?}"
    );
    assert_eq!(report.corrupt.len(), 1);
    // 0xEE is no valid tag: the codec names the exact refusal (BadTag) and the
    // verifier surfaces it as the corruption reason.
    assert!(
        report.corrupt[0].error.contains("BadTag"),
        "names the decode failure: {}",
        report.corrupt[0].error
    );
}

/// The verifier checks VALUES, not just keys: a base-relation row whose KEY
/// still decodes but whose VALUE is corrupt (an invalid polarity byte) is
/// caught, and the reason names the value failure — proof that catalog-aware
/// per-format value verification is real, not decorative.
#[test]
fn verify_storage_catches_a_corrupt_value() {
    use crate::runtime::db::Db;
    use crate::storage::verify::verify_storage;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    // Build a real store, capture a base-relation data-row key, then DROP the
    // store so the raw fjall handle below can take the file lock.
    let data_key: Vec<u8> = {
        let storage = new_fjall_storage(&path).unwrap();
        let db = Db::new(storage.clone()).unwrap();
        db.run_script(
            "?[k, v] <- [[1, 7]] :create rel {k => v}",
            std::collections::BTreeMap::new(),
        )
        .unwrap();
        let tx = storage.read_tx().unwrap();
        tx.total_scan()
            .filter_map(Result::ok)
            .map(|(k, _)| k.to_vec())
            // Any non-SYSTEM entry (relation prefix not all-zero): `rel` has no
            // index, so its base rows are the only non-catalog entries.
            .find(|k| k.len() >= 8 && k[..8].iter().any(|&b| b != 0))
            .expect("a rel base-relation data row")
    };
    // Overwrite that row's VALUE — not its key — with a single 0xFF byte, which
    // is no valid `ClaimPolarity`, below the kernel. The key still decodes; only
    // the value is corrupt, proving verify checks values, not just keys.
    {
        let raw = fjall::OptimisticTxDatabase::builder(&path).open().unwrap();
        let ks = raw
            .keyspace("kyzo", fjall::KeyspaceCreateOptions::default)
            .unwrap();
        ks.insert(&data_key, [0xFFu8]).unwrap();
        raw.persist(fjall::PersistMode::SyncAll).unwrap();
    }
    let storage = new_fjall_storage(&path).unwrap();
    let report = verify_storage(&storage).unwrap();
    assert!(
        !report.is_clean(),
        "a corrupt VALUE (with a still-decodable key) must be caught: {report:?}"
    );
    assert_eq!(
        report.corrupt.len(),
        1,
        "exactly the one wounded row: {report:?}"
    );
    assert!(
        report.corrupt[0].error.contains("polarity"),
        "the reason names the VALUE failure, not the key: {}",
        report.corrupt[0].error
    );
}

/// Law 6 (concurrency liveness) through the retry helper: contended
/// read-modify-write across threads completes exactly, with conflicts
/// retried rather than surfaced.
#[test]
fn retry_on_conflict_reaches_completion_under_contention() {
    use std::num::NonZeroUsize;
    use crate::storage::retry::retry_on_conflict;
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    {
        let mut tx = db.write_tx().unwrap();
        tx.put(b"n", b"0").unwrap();
        tx.commit().unwrap();
    }
    const THREADS: usize = 4;
    const OPS: usize = 20;
    std::thread::scope(|s| {
        for _ in 0..THREADS {
            let db = db.clone();
            s.spawn(move || {
                for _ in 0..OPS {
                    retry_on_conflict(NonZeroUsize::new(1_000).unwrap(), || {
                        let mut tx = db.write_tx()?;
                        let cur: u64 = std::str::from_utf8(&tx.get(b"n")?.unwrap())
                            .unwrap()
                            .parse()
                            .unwrap();
                        tx.put(b"n", (cur + 1).to_string().as_bytes())?;
                        { let _ = tx.commit()?; Ok(()) }
                    })
                    .unwrap();
                }
            });
        }
    });
    let tx = db.read_tx().unwrap();
    let total: u64 = std::str::from_utf8(&tx.get(b"n").unwrap().unwrap())
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(total, (THREADS * OPS) as u64);
}

/// Format-version stamps accept only canonical spellings: "01" and "+1"
/// parse numerically but are bytes no version of this code ever wrote.
#[test]
fn format_version_rejects_noncanonical_stamps() {
    use crate::storage::FormatVersion;
    assert_eq!(FormatVersion::parse(b"5").unwrap(), FormatVersion::CURRENT);
    // An older stamp still parses (so the mismatch refusal can NAME it) —
    // it is simply not CURRENT.
    assert_ne!(FormatVersion::parse(b"4").unwrap(), FormatVersion::CURRENT);
    for bad in [&b"01"[..], b"+1", b" 1", b"1 ", b""] {
        assert!(
            FormatVersion::parse(bad).is_err(),
            "must reject non-canonical stamp {bad:?}"
        );
    }
}

// ---------- deterministic simulation (DST) at the storage seam ----------
//
// SimStorage (storage/sim.rs) is the contract's own test double: schedules,
// faults, crashes, and power cuts are all pure functions of one seed. A
// failing campaign panics with "FAILING SEED = n"; rerunning with that seed
// replays the schedule and fault plan exactly.

use std::collections::BTreeSet;
use std::num::NonZeroUsize;

use crate::storage::retry::retry_on_conflict;
use crate::storage::sim::{
    FaultConfig, SimRng, SimStorage, TxBody, for_each_seed, run_interleaved,
};
use crate::data::value::data_value_any;

/// The sim must satisfy the same KV contract as the real backend: the mixed
/// workload from `kv_contract_matches_model`, checked against the BTreeMap
/// model oracle.
#[test]
fn sim_kv_contract_matches_model() {
    let db = SimStorage::new(0);
    let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    for round in 0u32..3 {
        let mut tx = db.write_tx().unwrap();
        for i in 0..40u32 {
            let n = (i * 7 + round * 13) % 50;
            let k = format!("k{n:03}").into_bytes();
            if n % 5 == round % 5 {
                tx.del(&k).unwrap();
                model.remove(&k);
            } else {
                let v = format!("v{round}-{n}").into_bytes();
                tx.put(&k, &v).unwrap();
                model.insert(k, v);
            }
        }
        if round == 2 {
            tx.del_range(b"k010", b"k020").unwrap();
            let doomed: Vec<_> = model
                .range(b"k010".to_vec()..b"k020".to_vec())
                .map(|(k, _)| k.clone())
                .collect();
            for k in doomed {
                model.remove(&k);
            }
        }
        tx.commit().unwrap();
    }

    let tx = db.read_tx().unwrap();
    let got: Vec<_> = tx.total_scan().map(|r| r.unwrap()).collect();
    let want: Vec<_> = model
        .iter()
        .map(|(k, v)| (Slice::from(k), Slice::from(v)))
        .collect();
    assert_eq!(got, want, "sim diverged from the model oracle");
    let got: Vec<_> = tx
        .range_scan(b"k005", b"k030")
        .map(|r| r.unwrap())
        .collect();
    let want: Vec<_> = model
        .range(b"k005".to_vec()..b"k030".to_vec())
        .map(|(k, v)| (Slice::from(k), Slice::from(v)))
        .collect();
    assert_eq!(got, want);
    assert_eq!(tx.range_count(b"k005", b"k030").unwrap(), want.len());
}

/// `SimWriteTx::range_scan`/`total_scan` (`visible_lazy`, the lazy
/// snapshot/write-overlay merge that replaced an eager per-call rebuild)
/// against an IN-FLIGHT, UNCOMMITTED transaction: every combination of
/// key placement the merge must get right — snapshot-only, write-only
/// (insert), present-in-both (write shadows snapshot), and a tombstone
/// over each of "had a snapshot entry" and "never existed" — checked mid
/// transaction, before commit, against a `BTreeMap` model of the same
/// overlay semantics.
#[test]
fn sim_write_tx_range_scan_overlay_matches_model() {
    let db = SimStorage::new(0xB17E);
    {
        let mut seed = db.write_tx().unwrap();
        for n in [0u32, 2, 4, 6, 8, 10, 12] {
            seed.put(format!("k{n:03}").as_bytes(), format!("snap{n}").as_bytes())
                .unwrap();
        }
        seed.commit().unwrap();
    }

    let mut tx = db.write_tx().unwrap();
    // Overwrite a snapshot key (write shadows snapshot).
    tx.put(b"k002", b"overwritten").unwrap();
    // Insert a fresh key the snapshot never had.
    tx.put(b"k003", b"fresh").unwrap();
    // Insert another fresh key, in between two existing ones.
    tx.put(b"k005", b"fresh2").unwrap();
    // Delete a snapshot key (tombstone shadowing a real entry).
    tx.del(b"k004").unwrap();
    // Delete a key that never existed (tombstone over nothing — a no-op
    // for the merge, must not appear and must not panic).
    tx.del(b"k999").unwrap();
    // k000, k006, k008, k010, k012 are untouched: snapshot-only survivors.

    let mut model: BTreeMap<Vec<u8>, Vec<u8>> = [0u32, 2, 4, 6, 8, 10, 12]
        .into_iter()
        .map(|n| {
            (
                format!("k{n:03}").into_bytes(),
                format!("snap{n}").into_bytes(),
            )
        })
        .collect();
    model.insert(b"k002".to_vec(), b"overwritten".to_vec());
    model.insert(b"k003".to_vec(), b"fresh".to_vec());
    model.insert(b"k005".to_vec(), b"fresh2".to_vec());
    model.remove(b"k004".as_slice());

    let got: Vec<_> = tx.total_scan().map(|r| r.unwrap()).collect();
    let want: Vec<_> = model
        .iter()
        .map(|(k, v)| (Slice::from(k), Slice::from(v)))
        .collect();
    assert_eq!(
        got, want,
        "total_scan mid-transaction diverged from the model"
    );

    // A bounded range spanning every case above, straddling both ends.
    let got: Vec<_> = tx
        .range_scan(b"k001", b"k011")
        .map(|r| r.unwrap())
        .collect();
    let want: Vec<_> = model
        .range(b"k001".to_vec()..b"k011".to_vec())
        .map(|(k, v)| (Slice::from(k), Slice::from(v)))
        .collect();
    assert_eq!(
        got, want,
        "range_scan mid-transaction diverged from the model"
    );

    tx.commit().unwrap();
}

/// The MVCC/SSI surface of the sim, point by point: typed conflicts on
/// read-write contention, write-write races aborting the second committer
/// (contract v2: writes are validated too), empty-write-set commits
/// certifying nothing, phantom protection on scanned ranges, snapshot
/// isolation, read-your-own-writes, del_range killing own writes,
/// uncommitted transactions leaving no trace, degenerate ranges.
#[test]
fn sim_mvcc_semantics_smoke() {
    use crate::storage::ConflictError;
    let db = SimStorage::new(1);
    {
        let mut tx = db.write_tx().unwrap();
        tx.put(b"counter", b"0").unwrap();
        tx.commit().unwrap();
    }
    // Read-write conflict is the typed, retryable error.
    let mut tx1 = db.write_tx().unwrap();
    let mut tx2 = db.write_tx().unwrap();
    assert_eq!(tx1.get(b"counter").unwrap(), Some(Slice::from(b"0")));
    assert_eq!(tx2.get(b"counter").unwrap(), Some(Slice::from(b"0")));
    tx1.put(b"counter", b"1").unwrap();
    tx2.put(b"counter", b"2").unwrap();
    tx1.commit().unwrap();
    let err = tx2.commit().unwrap_err();
    assert!(
        err.is_conflict(),
        "conflict must downcast to ConflictError, got {err:?}"
    );
    // Blind write-write on one key: writes are validated (contract v2), so
    // the second committer aborts with the typed conflict even though
    // neither side read — first-committer-wins, matching the real backend
    // (pinned there by `write_write_race_aborts_second_committer`). The
    // rerun on a fresh snapshot converges; disjoint blind writers still
    // never conflict.
    let mut tx1 = db.write_tx().unwrap();
    let mut tx2 = db.write_tx().unwrap();
    tx1.put(b"ww", b"1").unwrap();
    tx2.put(b"ww", b"2").unwrap();
    tx1.commit().expect("the FIRST committer must never abort");
    let err = tx2
        .commit()
        .expect_err("a write-write race must abort the second committer");
    assert!(
        err.is_conflict(),
        "the write-write abort must be the typed conflict, got {err:?}"
    );
    assert_eq!(
        db.read_tx().unwrap().get(b"ww").unwrap(),
        Some(Slice::from(b"1")),
        "the aborted writer must leave no trace: first committer wins"
    );
    let mut retry = db.write_tx().unwrap();
    retry.put(b"ww", b"2").unwrap();
    retry
        .commit()
        .expect("the rerun on a fresh snapshot commits");
    // del vs put on one key is a write-write race too.
    let mut tx1 = db.write_tx().unwrap();
    let mut tx2 = db.write_tx().unwrap();
    tx1.put(b"ww", b"3").unwrap();
    tx2.del(b"ww").unwrap();
    tx1.commit().unwrap();
    assert!(
        tx2.commit()
            .unwrap_err()
            .is_conflict(),
        "del vs put on one key must abort the second committer"
    );
    // Disjoint blind writers never conflict. (Keys chosen outside every
    // range this test scans later — "r…" would be a phantom in [r, s).)
    let mut tx1 = db.write_tx().unwrap();
    let mut tx2 = db.write_tx().unwrap();
    tx1.put(b"d-left", b"1").unwrap();
    tx2.put(b"d-right", b"2").unwrap();
    tx1.commit().expect("disjoint writers must not conflict");
    tx2.commit().expect("disjoint writers must not conflict");
    {
        let mut w = db.write_tx().unwrap();
        w.put(b"ww", b"2").unwrap();
        w.commit().unwrap();
    }
    // An empty write set commits vacuously — even when what it READ was
    // concurrently clobbered. A read-only WriteTx commit certifies nothing.
    let ro = db.write_tx().unwrap();
    assert_eq!(ro.get(b"ww").unwrap(), Some(Slice::from(b"2")));
    let mut w = db.write_tx().unwrap();
    w.put(b"ww", b"3").unwrap();
    w.commit().unwrap();
    ro.commit()
        .expect("an empty-write-set commit never aborts, clobbered reads or not");
    // Phantom protection: a scanned range is conflict-tracked.
    let mut tx1 = db.write_tx().unwrap();
    assert_eq!(tx1.range_scan(b"r", b"s").count(), 0);
    let mut tx2 = db.write_tx().unwrap();
    tx2.put(b"r-phantom", b"x").unwrap();
    tx2.commit().unwrap();
    tx1.put(b"elsewhere", b"y").unwrap();
    assert!(
        tx1.commit().is_err(),
        "insert into a scanned range must abort the scanner"
    );
    // Snapshot isolation + RYOW.
    let reader_before = db.read_tx().unwrap();
    let mut w = db.write_tx().unwrap();
    w.put(b"x", b"1").unwrap();
    assert_eq!(w.get(b"x").unwrap(), Some(Slice::from(b"1")), "RYOW");
    assert!(w.exists(b"x").unwrap());
    w.commit().unwrap();
    assert_eq!(reader_before.get(b"x").unwrap(), None, "snapshot isolation");
    assert_eq!(
        db.read_tx().unwrap().get(b"x").unwrap(),
        Some(Slice::from(b"1"))
    );
    // Uncommitted transactions leave no trace.
    {
        let mut tx = db.write_tx().unwrap();
        tx.put(b"ghost", b"boo").unwrap();
        // dropped, never committed
    }
    assert!(!db.read_tx().unwrap().exists(b"ghost").unwrap());
    // del_range kills own writes too; degenerate ranges are empty, no panic.
    let mut tx = db.write_tx().unwrap();
    tx.put(b"k1", b"1").unwrap();
    tx.put(b"z-outside", b"stays").unwrap();
    tx.del_range(b"k0", b"k9").unwrap();
    assert_eq!(tx.range_scan(b"z", b"a").count(), 0, "inverted range");
    assert_eq!(tx.range_scan(b"m", b"m").count(), 0, "empty range");
    tx.del_range(b"z", b"a").unwrap();
    tx.commit().unwrap();
    let tx = db.read_tx().unwrap();
    assert_eq!(tx.get(b"k1").unwrap(), None, "own write in range dies");
    assert_eq!(tx.get(b"z-outside").unwrap(), Some(Slice::from(b"stays")));
}

/// Spurious-conflict legality: with the injector at 100%, a commit with zero
/// contention still fails — as the *typed* ConflictError — and discards the
/// write set completely. SSI permits false positives; callers must retry.
#[test]
fn sim_spurious_conflict_is_typed_and_discards() {
    use crate::storage::ConflictError;
    let db = SimStorage::with_faults(
        4,
        FaultConfig {
            spurious_conflict_ppm: 1_000_000,
            ..Default::default()
        },
    );
    let mut tx = db.write_tx().unwrap();
    tx.put(b"a", b"1").unwrap();
    let err = tx.commit().unwrap_err();
    assert!(
        err.is_conflict(),
        "spurious conflict must be indistinguishable from a real one: {err:?}"
    );
    assert!(
        db.read_tx().unwrap().total_scan().next().is_none(),
        "a conflicted commit must leave no trace"
    );
}

/// The sim's seek-based as-of scan against the same naive oracle the real
/// backend is held to, plus own-writes visibility inside a write tx.
#[test]
fn sim_time_travel_matches_naive_oracle() {
    let history: &[(&str, i64, bool)] = &[
        ("a", 1, true),
        ("a", 3, true),
        ("a", 5, false),
        ("a", 7, true),
        ("b", 2, true),
        ("b", 6, false),
        ("c", 4, false),
        ("d", 9, true),
        ("e", 1, true),
        ("e", 2, false),
        ("e", 3, true),
        ("e", 4, false),
    ];
    let rel = RelationId::new(7).expect("below cap");
    let db = SimStorage::new(2);
    let mut tx = db.write_tx().unwrap();
    for (name, ts, assert) in history {
        let (k, v) = vld_row(rel, name, *ts, *assert);
        tx.put(&k, &v).unwrap();
    }
    // Own writes are visible to the write transaction's as-of scan.
    let lower = rel.raw_encode().to_vec();
    let upper = rel.next().expect("below cap").raw_encode().to_vec();
    assert!(
        tx.range_skip_scan_tuple(&lower, &upper, AsOf::current(ValidityTs::from_raw(1)))
            .next()
            .is_some(),
        "as-of scan must see own writes"
    );
    tx.commit().unwrap();

    let tx = db.read_tx().unwrap();
    for at in 0..=10i64 {
        let got: Vec<(String, i64)> = tx
            .range_skip_scan_tuple(&lower, &upper, AsOf::current(ValidityTs::from_raw(at)))
            .map(|r| {
                let t = r.unwrap();
                let name = match &t.as_slice()[0] {
                    DataValue::Str(s) => s.to_string(),
                    v @ (data_value_any!()) => panic!("unexpected {v:?}"),
                };
                let ts = match &t.as_slice()[1] {
                    DataValue::Validity(v) => v.ts_micros(),
                    v @ (data_value_any!()) => panic!("unexpected {v:?}"),
                };
                (name, ts)
            })
            .collect();
        assert_eq!(
            got,
            as_of_oracle(history, at),
            "as-of {at}: sim seek scan diverged from the naive oracle"
        );
    }
    // The sentinel edge: a MIN/MIN retraction must not livelock.
    let db = SimStorage::new(3);
    let mut tx = db.write_tx().unwrap();
    let (k, v) = vld_row(rel, "a", 1, true);
    tx.put(&k, &v).unwrap();
    tx.put(
        &bitemp_key(rel, "z", i64::MIN, i64::MIN),
        &pol_val(rel, false),
    )
    .unwrap();
    tx.commit().unwrap();
    let tx = db.read_tx().unwrap();
    let got = tx
        .range_skip_scan_tuple(&lower, &upper, AsOf::current(ValidityTs::from_raw(5)))
        .count();
    assert_eq!(got, 1, "scan must terminate and skip the MIN-ts retraction");
}

/// batch_put in atomic chunks: a full import lands complete; an import whose
/// iterator fails mid-stream leaves exactly the committed chunk prefix —
/// never a torn chunk. (The clean-prefix law at the batch_put seam.)
#[test]
fn sim_batch_put_atomic_chunks_and_clean_prefix() {
    let db = SimStorage::new(5);
    let n: u32 = 3000; // crosses the 1024-item chunk boundary twice
    let iter = (0..n).map(|i| {
        Ok((
            format!("k{i:08}").into_bytes(),
            format!("v{i}").into_bytes(),
        ))
    });
    db.batch_put(Box::new(iter)).unwrap();
    let tx = db.read_tx().unwrap();
    assert_eq!(tx.range_count(b"k", b"l").unwrap(), n as usize);
    assert_eq!(tx.get(b"k00002999").unwrap(), Some(Slice::from(b"v2999")));

    // An iterator error at item 2500 (inside the third chunk): the two
    // fully committed chunks survive, the torn chunk does not.
    let db = SimStorage::new(6);
    let iter = (0..n).map(|i| {
        if i == 2500 {
            Err(miette::miette!("sim: source failed mid-import"))
        } else {
            Ok((format!("k{i:08}").into_bytes(), b"v".to_vec()))
        }
    });
    assert!(db.batch_put(Box::new(iter)).is_err());
    let tx = db.read_tx().unwrap();
    assert_eq!(
        tx.range_count(b"k", b"l").unwrap(),
        2048,
        "an interrupted import must leave a clean prefix of whole chunks"
    );
}

/// Read faults are transient and seed-deterministic: the same seed replays
/// the identical fault pattern; successful reads return correct data; at
/// 100% every read-side operation fails.
#[test]
fn sim_read_faults_transient_and_deterministic() {
    let cfg = FaultConfig {
        read_fail_ppm: 250_000,
        ..Default::default()
    };
    let observe = |seed: u64| -> Vec<bool> {
        let db = SimStorage::with_faults(seed, cfg);
        let mut tx = db.write_tx().unwrap();
        tx.put(b"k", b"v").unwrap();
        tx.commit().unwrap();
        let tx = db.read_tx().unwrap();
        (0..200)
            .map(|_| match tx.get(b"k") {
                Ok(v) => {
                    assert_eq!(
                        v,
                        Some(Slice::from(b"v")),
                        "a non-faulted read must be correct"
                    );
                    false
                }
                Err(_) => true,
            })
            .collect()
    };
    let a = observe(11);
    assert_eq!(a, observe(11), "same seed must replay the same fault plan");
    assert!(a.contains(&true), "faults must fire at 25% over 200 reads");
    assert!(
        a.contains(&false),
        "faults must be transient, not permanent"
    );

    let always = SimStorage::with_faults(
        3,
        FaultConfig {
            read_fail_ppm: 1_000_000,
            ..Default::default()
        },
    );
    let mut tx = always.write_tx().unwrap();
    tx.put(b"k", b"v").unwrap();
    tx.commit().unwrap();
    let tx = always.read_tx().unwrap();
    assert!(tx.get(b"k").is_err());
    assert!(tx.exists(b"k").is_err());
    assert!(tx.range_scan(b"", b"z").next().unwrap().is_err());
    assert!(tx.total_scan().next().unwrap().is_err());
}

/// The schedule driver is seed-faithful in both directions: the same seed
/// replays the same interleaving bit-for-bit, and different seeds explore
/// genuinely different interleavings. The order-sensitive artifact is a log
/// key each transaction appends its id to under retry — its final content IS
/// the commit order, and its length proves no update was lost.
#[test]
fn sim_interleaving_seed_deterministic_and_diverse() {
    let run = |seed: u64| -> Vec<u8> {
        let db = SimStorage::new(seed);
        {
            let mut tx = db.write_tx().unwrap();
            tx.put(b"log", b"").unwrap();
            tx.commit().unwrap();
        }
        let bodies: Vec<TxBody<'_>> = (0..3u8)
            .map(|id| {
                let db = db.clone();
                Box::new(move || {
                    for _ in 0..2 {
                        retry_on_conflict(NonZeroUsize::new(10_000).unwrap(), || {
                            let mut tx = db.write_tx()?;
                            let mut log = tx.get(b"log")?.unwrap().to_vec();
                            log.push(b'0' + id);
                            tx.put(b"log", &log)?;
                            { let _ = tx.commit()?; Ok(()) }
                        })
                        .unwrap();
                    }
                }) as TxBody<'_>
            })
            .collect();
        run_interleaved(&db, seed, bodies);
        db.read_tx().unwrap().get(b"log").unwrap().unwrap().to_vec()
    };
    assert_eq!(run(5), run(5), "same seed must replay the same schedule");
    let outcomes: BTreeSet<Vec<u8>> = (0..20).map(run).collect();
    assert!(
        outcomes.len() > 1,
        "20 seeds must explore more than one interleaving, all gave {outcomes:?}"
    );
    for log in &outcomes {
        assert_eq!(log.len(), 6, "every append must land exactly once: {log:?}");
        for id in [b'0', b'1', b'2'] {
            assert_eq!(
                log.iter().filter(|c| **c == id).count(),
                2,
                "writer {id} must land exactly twice in {log:?}"
            );
        }
    }
}

/// Campaign (a): retry_on_conflict survives seeded storms of spurious
/// conflicts AND real contention from adversarial interleavings — always
/// terminating, with the final state exactly matching the model oracle.
#[test]
fn sim_campaign_retry_survives_spurious_conflicts_and_interleavings() {
    const BODIES: usize = 3;
    const OPS: usize = 4;
    for_each_seed(0..1000, |seed| {
        let db = SimStorage::with_faults(
            seed,
            FaultConfig {
                spurious_conflict_ppm: 200_000, // every 5th commit lies
                ..Default::default()
            },
        );
        retry_on_conflict(NonZeroUsize::new(10_000).unwrap(), || {
            let mut tx = db.write_tx()?;
            tx.put(b"counter", b"0")?;
            { let _ = tx.commit()?; Ok(()) }
        })
        .unwrap();

        let bodies: Vec<TxBody<'_>> = (0..BODIES)
            .map(|b| {
                let db = db.clone();
                Box::new(move || {
                    for i in 0..OPS {
                        retry_on_conflict(NonZeroUsize::new(10_000).unwrap(), || {
                            let mut tx = db.write_tx()?;
                            let cur: u64 = std::str::from_utf8(&tx.get(b"counter")?.unwrap())
                                .unwrap()
                                .parse()
                                .unwrap();
                            tx.put(b"counter", (cur + 1).to_string().as_bytes())?;
                            tx.put(format!("b{b}-k{i}").as_bytes(), b"x")?;
                            { let _ = tx.commit()?; Ok(()) }
                        })
                        .unwrap();
                    }
                }) as TxBody<'_>
            })
            .collect();
        run_interleaved(&db, seed, bodies);

        let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        model.insert(b"counter".to_vec(), (BODIES * OPS).to_string().into_bytes());
        for b in 0..BODIES {
            for i in 0..OPS {
                model.insert(format!("b{b}-k{i}").into_bytes(), b"x".to_vec());
            }
        }
        let got: Vec<_> = db
            .read_tx()
            .unwrap()
            .total_scan()
            .map(|r| r.unwrap())
            .collect();
        let want: Vec<_> = model
            .into_iter()
            .map(|(k, v)| (Slice::from(k), Slice::from(v)))
            .collect();
        assert_eq!(
            got, want,
            "final state diverged from the model: an increment was lost or doubled"
        );
    });
}

/// Campaign (b): crash consistency. For a seeded plan of K commits, a
/// simulated process crash at EVERY injection point (after 0..=K commits,
/// with a further uncommitted transaction staged at the crash) must leave
/// exactly the clean prefix of committed transactions — never a torn one,
/// never a trace of the uncommitted.
#[test]
fn sim_campaign_crash_is_clean_prefix_at_every_point() {
    const K: usize = 6;
    for_each_seed(0..150, |seed| {
        let mut rng = SimRng::new(seed ^ 0xC0A5_7A11);
        let plan: Vec<Vec<(Vec<u8>, Option<Vec<u8>>)>> = (0..K)
            .map(|c| {
                (0..4)
                    .map(|_| {
                        let k = format!("k{:02}", rng.below(20)).into_bytes();
                        if rng.below(4) == 0 {
                            (k, None)
                        } else {
                            (k, Some(format!("v{c}-{}", rng.below(100)).into_bytes()))
                        }
                    })
                    .collect()
            })
            .collect();

        for crash_after in 0..=K {
            let db = SimStorage::new(seed);
            let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
            for commit in &plan[..crash_after] {
                let mut tx = db.write_tx().unwrap();
                for (k, v) in commit {
                    match v {
                        Some(v) => {
                            tx.put(k, v).unwrap();
                            model.insert(k.clone(), v.clone());
                        }
                        None => {
                            tx.del(k).unwrap();
                            model.remove(k);
                        }
                    }
                }
                tx.commit().unwrap();
            }
            // Stage an uncommitted transaction right at the crash point.
            let mut staged = db.write_tx().unwrap();
            staged.put(b"zz-uncommitted", b"must-vanish").unwrap();
            let reopened = db.sim_crash();
            drop(staged);

            let got: Vec<_> = reopened
                .read_tx()
                .unwrap()
                .total_scan()
                .map(|r| r.unwrap())
                .collect();
            let want: Vec<_> = model
                .into_iter()
                .map(|(k, v)| (Slice::from(k), Slice::from(v)))
                .collect();
            assert_eq!(
                got, want,
                "crash after {crash_after}/{K} commits must leave exactly that prefix"
            );
        }
    });
}

/// Campaign (c): the two durability tiers are DISTINCT and both testable.
/// Buffer-tier commits survive a process crash but not a power cut; the
/// fsync tier (sync / commit_durable, SyncAll semantics) survives both. A
/// failed injected fsync leaves the commit applied but not power-cut
/// durable — the exact commit-then-persist shape of the real backend.
#[test]
fn sim_campaign_durability_tiers_are_distinct() {
    // Deterministic arm: c1 commit, sync, c2 commit, c3 commit_durable
    // (covers c2 via SyncAll), c4 commit (buffered only).
    let db = SimStorage::new(1);
    let put_commit = |k: &[u8], durable: bool| {
        let mut tx = db.write_tx().unwrap();
        tx.put(k, b"v").unwrap();
        if durable {
            tx.commit_durable().unwrap();
        } else {
            tx.commit().unwrap();
        }
    };
    put_commit(b"c1", false);
    db.sync().unwrap();
    put_commit(b"c2", false);
    put_commit(b"c3", true);
    put_commit(b"c4", false);
    let crashed = db.sim_crash().read_tx().unwrap();
    for k in [&b"c1"[..], b"c2", b"c3", b"c4"] {
        assert!(
            crashed.exists(k).unwrap(),
            "{k:?}: every commit survives a process crash"
        );
    }
    let cut = db.sim_powercut().read_tx().unwrap();
    for k in [&b"c1"[..], b"c2", b"c3"] {
        assert!(
            cut.exists(k).unwrap(),
            "{k:?}: fsynced commits survive a power cut"
        );
    }
    assert!(
        !cut.exists(b"c4").unwrap(),
        "a buffer-tier commit after the last fsync must NOT survive a power cut"
    );

    // A failed fsync: committed, crash-survivable, power-cut-lost.
    let db = SimStorage::with_faults(
        2,
        FaultConfig {
            sync_fail_ppm: 1_000_000,
            ..Default::default()
        },
    );
    let mut tx = db.write_tx().unwrap();
    tx.put(b"k", b"v").unwrap();
    assert!(tx.commit_durable().is_err(), "the fsync step must fail");
    assert!(db.sync().is_err(), "sync must fail too");
    assert!(
        db.sim_crash().read_tx().unwrap().exists(b"k").unwrap(),
        "the commit itself was applied"
    );
    assert!(
        !db.sim_powercut().read_tx().unwrap().exists(b"k").unwrap(),
        "but it never became power-cut durable"
    );

    // Seeded arm: a random mix of buffered commits, durable commits, and
    // syncs; crash must equal the all-commits model, power cut must equal
    // the model as of the last successful fsync barrier.
    for_each_seed(0..200, |seed| {
        let db = SimStorage::new(seed);
        let mut rng = SimRng::new(seed ^ 0xD0_4A81);
        let mut all: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        let mut synced: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        for i in 0..12u32 {
            match rng.below(4) {
                0 => {
                    db.sync().unwrap();
                    synced = all.clone();
                }
                1 => {
                    let k = format!("k{i}").into_bytes();
                    let mut tx = db.write_tx().unwrap();
                    tx.put(&k, b"durable").unwrap();
                    tx.commit_durable().unwrap();
                    all.insert(k, b"durable".to_vec());
                    synced = all.clone();
                }
                _ => {
                    let k = format!("k{i}").into_bytes();
                    let mut tx = db.write_tx().unwrap();
                    tx.put(&k, b"buffered").unwrap();
                    tx.commit().unwrap();
                    all.insert(k, b"buffered".to_vec());
                }
            }
        }
        let crash: Vec<_> = db
            .sim_crash()
            .read_tx()
            .unwrap()
            .total_scan()
            .map(|r| r.unwrap())
            .collect();
        let want_all: Vec<_> = all
            .into_iter()
            .map(|(k, v)| (Slice::from(k), Slice::from(v)))
            .collect();
        assert_eq!(crash, want_all, "crash must keep every commit");
        let cut: Vec<_> = db
            .sim_powercut()
            .read_tx()
            .unwrap()
            .total_scan()
            .map(|r| r.unwrap())
            .collect();
        let want_synced: Vec<_> = synced
            .into_iter()
            .map(|(k, v)| (Slice::from(k), Slice::from(v)))
            .collect();
        assert_eq!(
            cut, want_synced,
            "power cut must keep exactly the fsynced prefix"
        );
    });
}

/// Campaign (d): as-of/time-travel reads stay correct when the history is
/// written by interleaved, retrying transactions under spurious conflicts —
/// for every seed, the final seek-based as-of scan must match the naive
/// oracle at every timestamp.
#[test]
fn sim_campaign_time_travel_under_interleaved_history_writes() {
    let rel = RelationId::new(9).expect("below cap");
    for_each_seed(0..200, |seed| {
        let db = SimStorage::with_faults(
            seed,
            FaultConfig {
                spurious_conflict_ppm: 100_000,
                ..Default::default()
            },
        );
        // A seeded history, deduplicated on (name, ts) so the oracle's
        // tie-break never diverges from key order, split across 3 writers.
        // INVARIANT(test_seed_mix): property-test seed diffusion uses modular golden mix.
        let mut rng = SimRng::new(seed.wrapping_mul(0x9E37_79B9).wrapping_add(1));
        let mut seen = BTreeSet::new();
        let mut history: Vec<(String, i64, bool)> = vec![];
        let mut plans: Vec<Vec<(String, i64, bool)>> = vec![vec![]; 3];
        for i in 0..12usize {
            let name = format!("e{}", rng.below(4));
            let ts = rng.below(10) as i64;
            if !seen.insert((name.clone(), ts)) {
                continue;
            }
            let is_assert = rng.below(3) != 0;
            plans[i % 3].push((name.clone(), ts, is_assert));
            history.push((name, ts, is_assert));
        }
        let bodies: Vec<TxBody<'_>> = plans
            .into_iter()
            .map(|plan| {
                let db = db.clone();
                Box::new(move || {
                    for (name, ts, a) in plan {
                        retry_on_conflict(NonZeroUsize::new(10_000).unwrap(), || {
                            let mut tx = db.write_tx()?;
                            let (k, v) = vld_row(rel, &name, ts, a);
                            tx.put(&k, &v)?;
                            { let _ = tx.commit()?; Ok(()) }
                        })
                        .unwrap();
                    }
                }) as TxBody<'_>
            })
            .collect();
        run_interleaved(&db, seed, bodies);

        let hist_refs: Vec<(&str, i64, bool)> = history
            .iter()
            .map(|(n, t, a)| (n.as_str(), *t, *a))
            .collect();
        let lower = rel.raw_encode().to_vec();
        let upper = rel.next().expect("below cap").raw_encode().to_vec();
        let tx = db.read_tx().unwrap();
        for at in 0..=10i64 {
            let got: Vec<(String, i64)> = tx
                .range_skip_scan_tuple(&lower, &upper, AsOf::current(ValidityTs::from_raw(at)))
                .map(|r| {
                    let t = r.unwrap();
                    let name = match &t.as_slice()[0] {
                        DataValue::Str(s) => s.to_string(),
                        v @ (data_value_any!()) => panic!("unexpected {v:?}"),
                    };
                    let ts = match &t.as_slice()[1] {
                        DataValue::Validity(v) => v.ts_micros(),
                        v @ (data_value_any!()) => panic!("unexpected {v:?}"),
                    };
                    (name, ts)
                })
                .collect();
            assert_eq!(
                got,
                as_of_oracle(&hist_refs, at),
                "as-of {at}: interleaved history diverged from the oracle"
            );
        }
    });
}

/// Campaign (e): WRITE SKEW is aborted and serialized. A reads x and writes
/// y; B reads y and writes x — the canonical anomaly that write-write-only
/// validation cannot see, because the write sets are disjoint. Both
/// transactions are opened before the schedule runs, so their snapshots
/// always overlap; whichever commits second has READ a key the first
/// committer wrote and must abort with the typed conflict. The aborted side
/// retries on a fresh snapshot, so the final state must be one of the two
/// SERIAL outcomes — never the skew anomaly (x=1, y=1).
#[test]
fn sim_campaign_write_skew_aborts_and_serializes() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::storage::ConflictError;

    let parse = |v: Option<Slice>| -> u64 {
        std::str::from_utf8(&v.expect("key must exist"))
            .unwrap()
            .parse()
            .unwrap()
    };
    for_each_seed(0..200, |seed| {
        let db = SimStorage::new(seed);
        {
            let mut tx = db.write_tx().unwrap();
            tx.put(b"x", b"0").unwrap();
            tx.put(b"y", b"0").unwrap();
            tx.commit().unwrap();
        }
        let aborts = AtomicUsize::new(0);
        // Opened BEFORE the schedule runs: overlapping snapshots in every
        // seed, so serializability demands an abort in every seed.
        let tx_a = db.write_tx().unwrap();
        let tx_b = db.write_tx().unwrap();
        let bodies: Vec<TxBody<'_>> = [(tx_a, &b"x"[..], &b"y"[..]), (tx_b, b"y", b"x")]
            .into_iter()
            .map(|(mut tx, src, dst)| {
                let db = db.clone();
                let (aborts, parse) = (&aborts, &parse);
                Box::new(move || {
                    let n = parse(tx.get(src).unwrap());
                    tx.put(dst, (n + 1).to_string().as_bytes()).unwrap();
                    if let Err(e) = tx.commit() {
                        assert!(
                            e.is_conflict(),
                            "only the typed conflict is a legal abort: {e:?}"
                        );
                        aborts.fetch_add(1, Ordering::Relaxed);
                        retry_on_conflict(NonZeroUsize::new(10_000).unwrap(), || {
                            let mut tx = db.write_tx()?;
                            let n = parse(tx.get(src)?);
                            tx.put(dst, (n + 1).to_string().as_bytes())?;
                            { let _ = tx.commit()?; Ok(()) }
                        })
                        .unwrap();
                    }
                }) as TxBody<'_>
            })
            .collect();
        run_interleaved(&db, seed, bodies);

        assert!(
            aborts.load(Ordering::Relaxed) >= 1,
            "write skew: overlapping snapshots with crossed read/write sets \
             must abort at least one side in EVERY seed"
        );
        let r = db.read_tx().unwrap();
        let (x, y) = (parse(r.get(b"x").unwrap()), parse(r.get(b"y").unwrap()));
        assert!(
            (x, y) == (2, 1) || (x, y) == (1, 2),
            "final state must be one of the two serial outcomes, \
             got x={x} y={y} (x=1 y=1 is the write-skew anomaly)"
        );
    });
}

/// Campaign (f): NO LOST PHANTOM under adversarial interleaving. A
/// range-scans [p, q) and writes a summary of what it saw; B blind-inserts a
/// phantom into A's scanned range. Both transactions are opened before the
/// schedule runs, so A's snapshot always predates B's insert. Commit order
/// is observed through the (serialized) scheduler: if B committed first, A's
/// scanned range was modified post-snapshot and A MUST have aborted and
/// re-scanned — a summary of 3 committed after B's insert is the lost
/// phantom this campaign exists to catch.
#[test]
fn sim_campaign_no_lost_phantom_under_interleaving() {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    for_each_seed(0..200, |seed| {
        let db = SimStorage::new(seed);
        {
            let mut tx = db.write_tx().unwrap();
            for i in 0..3 {
                tx.put(format!("p{i}").as_bytes(), b"v").unwrap();
            }
            tx.commit().unwrap();
        }
        let a_aborts = AtomicUsize::new(0);
        let order: Mutex<Vec<&'static str>> = Mutex::new(vec![]);
        let mut tx_a = db.write_tx().unwrap();
        let mut tx_b = db.write_tx().unwrap();

        let body_a = {
            let db = db.clone();
            let (a_aborts, order) = (&a_aborts, &order);
            Box::new(move || {
                let n = tx_a.range_scan(b"p", b"q").count();
                assert_eq!(n, 3, "A's snapshot predates B's insert");
                tx_a.put(b"summary", n.to_string().as_bytes()).unwrap();
                if tx_a.commit().is_err() {
                    a_aborts.fetch_add(1, Ordering::Relaxed);
                    retry_on_conflict(NonZeroUsize::new(10_000).unwrap(), || {
                        let mut tx = db.write_tx()?;
                        let n = tx.range_scan(b"p", b"q").count();
                        tx.put(b"summary", n.to_string().as_bytes())?;
                        { let _ = tx.commit()?; Ok(()) }
                    })
                    .unwrap();
                }
                // Not a sim op, so it runs inside the same scheduler turn as
                // the successful commit above: push order == commit order.
                order.lock().unwrap().push("A");
            }) as TxBody<'_>
        };
        let body_b = {
            let order = &order;
            Box::new(move || {
                tx_b.put(b"p-new", b"v").unwrap();
                // B never conflicts: its write set ({p-new}) is disjoint
                // from every concurrent write (A writes only "summary"), and
                // B reads nothing beyond the key it wrote. Under contract v2
                // even a written key is validated — but nobody else touches
                // p-new, so the validation passes in every schedule.
                tx_b.commit()
                    .expect("B's writes race with nothing: never conflicts");
                order.lock().unwrap().push("B");
            }) as TxBody<'_>
        };
        run_interleaved(&db, seed, vec![body_a, body_b]);

        let order = order.lock().unwrap().clone();
        let r = db.read_tx().unwrap();
        assert_eq!(
            r.range_count(b"p", b"q").unwrap(),
            4,
            "B's phantom insert must have landed"
        );
        let summary: usize = std::str::from_utf8(&r.get(b"summary").unwrap().unwrap())
            .unwrap()
            .parse()
            .unwrap();
        match order.as_slice() {
            ["B", "A"] => {
                assert_eq!(
                    summary, 4,
                    "LOST PHANTOM: A committed a stale summary after B's \
                     insert into A's scanned range (seed {seed})"
                );
                assert!(
                    a_aborts.load(Ordering::Relaxed) >= 1,
                    "A committed after B and must have aborted+rescanned first"
                );
            }
            ["A", "B"] => {
                assert_eq!(summary, 3, "A serialized first: summary is pre-phantom");
                assert_eq!(
                    a_aborts.load(Ordering::Relaxed),
                    0,
                    "A committed first: nothing can have aborted it"
                );
            }
            other => panic!("both bodies must commit exactly once, got {other:?}"),
        }
    });
}

/// Campaign (g): WRITE-WRITE races are first-committer-wins under
/// adversarial interleaving (contract v2). Two transactions with overlapping
/// snapshots (both opened before the schedule runs) blind-put the SAME key —
/// no reads anywhere, so under the old reads-only contract neither would
/// ever abort and the race merged silently as last-writer-wins. Now the
/// second committer must abort with the typed conflict in EVERY seed, its
/// retry on a fresh snapshot must converge, and the loser's rerun aborting
/// again is impossible (the winner committed before the rerun's snapshot).
#[test]
fn sim_campaign_write_write_race_first_committer_wins() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::storage::ConflictError;

    for_each_seed(0..200, |seed| {
        let db = SimStorage::new(seed);
        let aborts = AtomicUsize::new(0);
        // Opened BEFORE the schedule runs: overlapping snapshots in every
        // seed, so write-set validation demands an abort in every seed.
        let tx_a = db.write_tx().unwrap();
        let tx_b = db.write_tx().unwrap();
        let bodies: Vec<TxBody<'_>> = [(tx_a, &b"A"[..]), (tx_b, b"B")]
            .into_iter()
            .map(|(mut tx, val)| {
                let db = db.clone();
                let aborts = &aborts;
                Box::new(move || {
                    tx.put(b"hot", val).unwrap();
                    if let Err(e) = tx.commit() {
                        assert!(
                            e.is_conflict(),
                            "only the typed conflict is a legal abort: {e:?}"
                        );
                        aborts.fetch_add(1, Ordering::Relaxed);
                        retry_on_conflict(NonZeroUsize::new(10_000).unwrap(), || {
                            let mut tx = db.write_tx()?;
                            tx.put(b"hot", val)?;
                            { let _ = tx.commit()?; Ok(()) }
                        })
                        .unwrap();
                    }
                }) as TxBody<'_>
            })
            .collect();
        run_interleaved(&db, seed, bodies);

        assert_eq!(
            aborts.load(Ordering::Relaxed),
            1,
            "overlapping snapshots writing one key: exactly the second \
             committer aborts, in every seed (seed {seed})"
        );
        let v = db.read_tx().unwrap().get(b"hot").unwrap().unwrap();
        assert!(
            v == b"A" || v == b"B",
            "the final value belongs to exactly one writer, got {v:?}"
        );
    });
}

/// The sim transactions satisfy the same compiler-checked concurrency
/// bounds as the real backend's.
#[test]
fn sim_concurrency_bounds_are_compiler_checked() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<SimStorage>();
    assert_send_sync::<crate::storage::sim::SimReadTx>();
    assert_send_sync::<crate::storage::sim::SimWriteTx>();
}

/// HARDENING SENTINEL — the determinism law of identity-keyed fault
/// injection: same seed => byte-identical fault schedule at ANY thread
/// count, scheduler or no scheduler. Fixed logical work (32 keys × 4 read
/// attempts; 32 one-key commit identities × 4 attempts) is partitioned
/// across 1/2/4/8 free-running OS threads — no token-barrier scheduler, so
/// arrival order at the shared state is genuinely nondeterministic — and
/// the observed (logical op, attempt) → fault outcome matrix must be
/// identical for every partitioning. A positional (global op-counter) fault
/// plan fails this test: OS arrival order would reshuffle the plan.
#[test]
fn sim_fault_plan_identical_at_any_thread_count() {
    const KEYS: usize = 32;
    const ATTEMPTS: usize = 4;
    type Matrix = Vec<Vec<bool>>;

    fn observe(seed: u64, threads: usize) -> (Matrix, Matrix) {
        let db = SimStorage::with_faults(
            seed,
            FaultConfig {
                read_fail_ppm: 400_000,
                spurious_conflict_ppm: 400_000,
                sync_fail_ppm: 0,
            },
        );
        // Populate the read keys (commits may draw spurious conflicts: retry).
        retry_on_conflict(NonZeroUsize::new(10_000).unwrap(), || {
            let mut tx = db.write_tx()?;
            for i in 0..KEYS {
                tx.put(format!("r{i:02}").as_bytes(), b"v")?;
            }
            { let _ = tx.commit()?; Ok(()) }
        })
        .unwrap();

        // Each key's identity is owned by exactly one thread (key i → thread
        // i % threads), so its attempt numbers 1..=ATTEMPTS are assigned
        // within one thread and the full matrix is comparable across runs.
        let mut reads: Matrix = vec![vec![]; KEYS];
        let mut commits: Matrix = vec![vec![]; KEYS];
        std::thread::scope(|s| {
            let handles: Vec<_> = (0..threads)
                .map(|t| {
                    let db = db.clone();
                    s.spawn(move || {
                        let mut out = Vec::new();
                        let tx = db.read_tx().unwrap();
                        for i in (t..KEYS).step_by(threads) {
                            let key = format!("r{i:02}").into_bytes();
                            let r: Vec<bool> =
                                (0..ATTEMPTS).map(|_| tx.get(&key).is_err()).collect();
                            // Commit stream: same one-key write set each
                            // attempt = same commit identity, attempts 1..=4.
                            // Blind disjoint writes never REALLY conflict, so
                            // an Err is exactly a spurious injection.
                            let c: Vec<bool> = (0..ATTEMPTS)
                                .map(|_| {
                                    let mut w = db.write_tx().unwrap();
                                    w.put(format!("c{i:02}").as_bytes(), b"v").unwrap();
                                    w.commit().is_err()
                                })
                                .collect();
                            out.push((i, r, c));
                        }
                        out
                    })
                })
                .collect();
            for h in handles {
                for (i, r, c) in h.join().unwrap() {
                    reads[i] = r;
                    commits[i] = c;
                }
            }
        });
        (reads, commits)
    }

    let base = observe(42, 1);
    let fired = |m: &Matrix| m.iter().flatten().any(|b| *b);
    let missed = |m: &Matrix| m.iter().flatten().any(|b| !*b);
    assert!(
        fired(&base.0) && missed(&base.0) && fired(&base.1) && missed(&base.1),
        "at 40% ppm over 128 draws each, both streams must contain hits and misses"
    );
    for threads in [2usize, 4, 8] {
        assert_eq!(
            base,
            observe(42, threads),
            "fault schedule must be byte-identical at {threads} threads"
        );
    }
    assert_ne!(
        base,
        observe(43, 1),
        "different seeds must explore different fault schedules"
    );
}

/// RETRY LIVENESS by construction: a retried operation keeps its fault
/// identity but advances its per-identity attempt, drawing a fresh decision
/// each time — so even a 90% fault storm cannot pin a bounded retry loop to
/// a permanent fault. An identity scheme MISSING the attempt component
/// re-draws the identical decision forever: ~90% of these loops would
/// exhaust their bound (mutation-verified — this test must fail under that
/// mutant).
#[test]
fn sim_retry_liveness_escapes_injected_faults() {
    for_each_seed(0..20, |seed| {
        let db = SimStorage::with_faults(
            seed,
            FaultConfig {
                read_fail_ppm: 900_000,
                spurious_conflict_ppm: 900_000,
                sync_fail_ppm: 0,
            },
        );
        // Commit arm: same write-set keys each retry = same commit identity;
        // the attempt component must let every loop escape well within its
        // bound (P(1000 straight faults) = 0.9^1000 ≈ 10^-46).
        for i in 0..20 {
            retry_on_conflict(NonZeroUsize::new(1_000).unwrap(), || {
                let mut tx = db.write_tx()?;
                tx.put(format!("k{i}").as_bytes(), b"v")?;
                { let _ = tx.commit()?; Ok(()) }
            })
            .unwrap_or_else(|e| panic!("commit k{i} never escaped its injected conflict: {e}"));
        }
        // Read arm: a bounded re-read of one key = same read identity,
        // advancing attempts; it must succeed within the bound.
        let tx = db.read_tx().unwrap();
        let escaped = (0..1_000).any(|_| tx.get(b"k0").is_ok());
        assert!(escaped, "a re-read loop never escaped its injected fault");
    });
}
// probe: two overlapping write txs skip-scan the same fact range then write
// distinct version keys — SSI must abort the second committer.
#[test]
fn bitemporal_fact_race_aborts_second_committer() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let rel = RelationId::new(7).expect("below cap");
    // seed
    let mut tx = db.write_tx().unwrap();
    let (k, v) = vld_row(rel, "ctr", 0, true);
    tx.put(&k, &v).unwrap();
    tx.commit().unwrap();

    let lower = rel.raw_encode().to_vec();
    let upper = rel.next().expect("below cap").raw_encode().to_vec();
    let mut a = db.write_tx().unwrap();
    let mut b = db.write_tx().unwrap();
    // both read the fact range (the current-state probe)
    let _ = a
        .range_skip_scan_tuple(
            &lower,
            &upper,
            AsOf::current(ValidityTs::from_raw(i64::MAX)),
        )
        .count();
    let _ = b
        .range_skip_scan_tuple(
            &lower,
            &upper,
            AsOf::current(ValidityTs::from_raw(i64::MAX)),
        )
        .count();
    // both write DISTINCT version keys of the same fact
    let (ka, va) = (bitemp_key(rel, "ctr", 1, 100), pol_val(rel, true));
    let (kb, vb) = (bitemp_key(rel, "ctr", 1, 200), pol_val(rel, true));
    a.put(&ka, &va).unwrap();
    b.put(&kb, &vb).unwrap();
    a.commit().unwrap();
    let second = b.commit();
    assert!(
        second.is_err(),
        "second committer must abort: its read range was written by the first"
    );
}

// ---------- the system clock: monotone stamps, crash-safe floors ----------

/// Stamps are strictly monotone across transactions, and the fjall
/// watermark makes them survive a close-and-reopen: a store reopened at
/// the same path mints strictly above every stamp the previous handle
/// ever minted, whatever the wall clock does.
#[test]
fn system_stamps_survive_reopen_strictly_monotone() {
    let dir = tempfile::tempdir().unwrap();
    let mut last = None;
    {
        let db = new_fjall_storage(dir.path()).unwrap();
        for _ in 0..5 {
            let tx = db.write_tx().unwrap();
            let stamp = tx.system_stamp();
            if let Some(prev) = last {
                assert!(
                    stamp < prev,
                    "stamps strictly monotone (Reverse: later is smaller)"
                );
            }
            last = Some(stamp);
            // No commit: even an abandoned transaction's mint raises the
            // floor — a too-high floor is safe, a reused stamp is not.
        }
    }
    let db = new_fjall_storage(dir.path()).unwrap();
    let tx = db.write_tx().unwrap();
    assert!(
        tx.system_stamp() < last.unwrap(),
        "a reopened store mints strictly above the previous handle's stamps"
    );
}

/// The sim's logical stamp floor rides through simulated crashes and
/// power cuts: post-recovery transactions mint strictly above every
/// pre-crash stamp, mirroring the fjall watermark's guarantee.
#[test]
fn sim_stamps_survive_crash_and_powercut() {
    let db = SimStorage::new(11);
    let mut pre = None;
    for _ in 0..3 {
        let tx = db.write_tx().unwrap();
        pre = Some(tx.system_stamp());
    }
    let crashed = db.sim_crash();
    let tx = crashed.write_tx().unwrap();
    assert!(
        tx.system_stamp() < pre.unwrap(),
        "crash keeps the stamp floor"
    );
    let cut = db.sim_powercut();
    let tx = cut.write_tx().unwrap();
    assert!(
        tx.system_stamp() < pre.unwrap(),
        "power cut keeps the stamp floor"
    );
}

/// The SSI increment law under real thread contention, at the storage
/// layer alone: two racers each skip-scan the fact's range on their WRITE
/// transaction and write a fresh version key; every commit that returns
/// `Ok` must be observed by every later reader. A lost update here is a
/// conflict-oracle hole, not a query-tier bug.
#[test]
fn concurrent_increments_lose_nothing_at_the_storage_layer() {
    use std::sync::atomic::{AtomicI64, Ordering};

    use crate::storage::ConflictError;

    let dir = tempfile::tempdir().unwrap();
    let db = std::sync::Arc::new(new_fjall_storage(dir.path()).unwrap());
    let rel = RelationId::new(7).expect("below cap");
    let lower = rel.raw_encode().to_vec();
    let upper = rel.next().expect("below cap").raw_encode().to_vec();

    // Assert value: the polarity byte, then the counter column canonical.
    let val_of = |v: i64| -> Vec<u8> {
        let mut out = Vec::new();
        out.push(crate::data::bitemporal::ClaimPolarity::Assert.encode());
        crate::data::value::append_canonical(&mut out, &DataValue::from(v));
        out
    };
    // Version key of the one fact at (valid=stamp, sys=stamp).
    let key_at = |stamp: ValidityTs| -> StorageKey {
        let slot = DataValue::Validity(Validity::from_stored(stamp, true));
        let tuple: Tuple = Tuple::from_vec(vec![DataValue::from(0), slot.clone(), slot]);
        tuple.encode_as_key(rel)
    };
    let current = |rows: Vec<Tuple>| -> i64 {
        assert_eq!(rows.len(), 1, "exactly one live fact, got {rows:?}");
        rows[0].last().unwrap().get_int().expect("counter int")
    };

    {
        let mut tx = db.write_tx().unwrap();
        let stamp = tx.system_stamp();
        tx.put(&key_at(stamp), &val_of(0)).unwrap();
        tx.commit().unwrap();
    }

    const PER_THREAD: i64 = 200;
    let commits = AtomicI64::new(0);
    std::thread::scope(|scope| {
        for _ in 0..2 {
            let db = db.clone();
            let commits = &commits;
            let (lower, upper) = (lower.clone(), upper.clone());
            let (val_of, key_at) = (&val_of, &key_at);
            scope.spawn(move || {
                for _ in 0..PER_THREAD {
                    loop {
                        let mut tx = db.write_tx().unwrap();
                        let stamp = tx.system_stamp();
                        let rows: Vec<Tuple> = tx
                            .range_skip_scan_tuple(
                                &lower,
                                &upper,
                                AsOf::current(ValidityTs::from_raw(i64::MAX)),
                            )
                            .map(|r| r.unwrap())
                            .collect();
                        let old = current(rows);
                        tx.put(&key_at(stamp), &val_of(old + 1)).unwrap();
                        match tx.commit() {
                            Ok(_committed) => {
                                commits.fetch_add(1, Ordering::SeqCst);
                                break;
                            }
                            Err(e) if e.is_conflict() => {
                                continue;
                            }
                            Err(e) => panic!("unexpected commit error: {e:?}"),
                        }
                    }
                }
            });
        }
    });

    let rtx = db.read_tx().unwrap();
    let rows: Vec<Tuple> = rtx
        .range_skip_scan_tuple(
            &lower,
            &upper,
            AsOf::current(ValidityTs::from_raw(i64::MAX)),
        )
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(
        current(rows),
        2 * PER_THREAD,
        "every Ok commit observed ({} commits)",
        commits.load(Ordering::SeqCst)
    );
}

/// Restore raises the target's clock floor to the dump's: stamps minted
/// after a restore are strictly above every instant in the imported
/// history, even when the source clock ran far ahead of this machine's
/// wall clock.
#[test]
fn restore_raises_clock_floor_past_imported_stamps() {
    let src_dir = tempfile::tempdir().unwrap();
    let src = new_fjall_storage(src_dir.path()).unwrap();
    // Push the source clock far into the future, then write one row so
    // the dump carries both data and the inflated floor.
    let far_future = crate::runtime::current_validity().unwrap().raw() + 1_000_000_000;
    src.raise_clock_floor(ValidityTs::from_raw(far_future))
        .unwrap();
    {
        let mut tx = src.write_tx().unwrap();
        let stamp = tx.system_stamp();
        assert!(stamp.raw() > far_future, "source mints above its floor");
        let (k, v) = vld_row(RelationId::new(3).expect("below cap"), "fact", 1, true);
        tx.put(&k, &v).unwrap();
        tx.commit().unwrap();
    }
    let dump = src_dir.path().join("dump.kyzo");
    crate::storage::backup::dump_storage(&src, &dump).unwrap();

    let dst_dir = tempfile::tempdir().unwrap();
    let dst = new_fjall_storage(dst_dir.path()).unwrap();
    crate::storage::backup::restore_storage(&dst, &dump).unwrap();
    let tx = dst.write_tx().unwrap();
    assert!(
        tx.system_stamp().raw() > far_future,
        "post-restore mints must exceed every imported instant"
    );
}

/// A dump truncated inside the fixed-width clock-floor field refuses
/// cleanly (typed error, no import).
#[test]
fn truncated_dump_missing_floor_bytes_is_refused() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("trunc.kyzo");
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(b"KYZODMP2").unwrap();
    let version = crate::storage::FormatVersion::CURRENT.as_bytes();
    f.write_all(&(version.len() as u64).to_be_bytes()).unwrap();
    f.write_all(&version).unwrap();
    f.write_all(&[0u8; 3]).unwrap(); // three of the floor's eight bytes
    drop(f);
    let db = new_fjall_storage(dir.path().join("store")).unwrap();
    let err = crate::storage::backup::restore_storage(&db, &path).unwrap_err();
    assert!(
        err.to_string().contains("missing clock floor"),
        "typed truncation refusal, got: {err}"
    );
    let rtx = db.read_tx().unwrap();
    assert!(rtx.total_scan().next().is_none(), "nothing imported");
}

/// Issue #118 task 4: `StorageOptions::cache_size_bytes` left `None` gets a
/// 25%-of-system-RAM floor on Linux (`quarter_system_ram_bytes`), not
/// fjall's own tiny stock default. On the Linux CI/dev boxes this runs on,
/// `/proc/meminfo` is always readable, so this pins the floor is a real,
/// positive, sane number rather than silently falling through.
#[test]
fn cache_floor_reads_a_real_ram_quarter_on_linux() {
    let quarter = crate::storage::fjall::quarter_system_ram_bytes()
        .expect("/proc/meminfo is readable on the Linux boxes this runs on");
    assert!(quarter > 0, "a real host has nonzero RAM");
    // Sanity band: no real host has under 64 MiB or over 1 PiB of RAM, so a
    // quarter of it lands well inside this range — catches a unit mixup
    // (e.g. forgetting the kB->bytes conversion) without pinning an exact
    // host-dependent value.
    assert!(
        (16 * 1024 * 1024..(1u64 << 58)).contains(&quarter),
        "quarter={quarter} outside the sane band — check the kB->bytes math"
    );
}
