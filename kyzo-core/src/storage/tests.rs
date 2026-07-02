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

use std::cmp::Reverse;
use std::collections::BTreeMap;

use proptest::prelude::*;

use crate::data::memcmp::MemCmpEncoder;
use crate::data::tuple::{EncodedKey, RelationId, Tuple, TupleT};
use crate::data::value::{DataValue, JsonData, Num, Validity, ValidityTs, Vector};
use crate::storage::backup::{dump_storage, restore_storage};
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
        DataValue::Num(Num::Float(f64::NEG_INFINITY)),
        DataValue::Num(Num::Int(i64::MIN)),
        DataValue::Num(Num::Int(-1_000_000)),
        DataValue::Num(Num::Float(-1.5)),
        DataValue::Num(Num::Int(-1)),
        DataValue::Num(Num::Float(-0.0)),
        DataValue::Num(Num::Int(0)),
        DataValue::Num(Num::Float(0.0)),
        DataValue::Num(Num::Float(0.5)),
        DataValue::Num(Num::Int(1)),
        DataValue::Num(Num::Float(1.0)), // int/float tie
        DataValue::Num(Num::Float(1.5)),
        DataValue::Num(Num::Int(2)),
        // The exact-int boundary: 2^53 ± 1 in both representations.
        DataValue::Num(Num::Int((1 << 53) - 1)),
        DataValue::Num(Num::Int(1 << 53)),
        DataValue::Num(Num::Int((1 << 53) + 1)),
        DataValue::Num(Num::Float((1u64 << 53) as f64)),
        DataValue::Num(Num::Int(i64::MAX)),
        DataValue::Num(Num::Float(f64::INFINITY)),
        DataValue::Num(Num::Float(f64::NAN)),
        DataValue::Str("".into()),
        DataValue::Str("a".into()),
        DataValue::Str("ab".into()),
        DataValue::Str("b".into()),
        DataValue::Str("Ω unicode ω".into()),
        DataValue::Bytes(vec![]),
        DataValue::Bytes(vec![0]),
        DataValue::Bytes(vec![0, 1]),
        DataValue::Bytes(vec![255]),
        DataValue::Uuid(crate::UuidWrapper(uuid::Uuid::from_u128(0))),
        DataValue::Uuid(crate::UuidWrapper(uuid::Uuid::from_u128(
            0x1234_5678_9abc_def0_1234_5678_9abc_def0,
        ))),
        DataValue::Regex(crate::RegexWrapper("^a.*b$".parse().unwrap())),
        DataValue::Regex(crate::RegexWrapper("^z+$".parse().unwrap())),
        DataValue::List(vec![]),
        DataValue::List(vec![DataValue::Num(Num::Int(1))]),
        // prefix ordering: [1] < [1, "x"]
        DataValue::List(vec![
            DataValue::Num(Num::Int(1)),
            DataValue::Str("x".into()),
        ]),
        DataValue::Set(Default::default()),
        DataValue::Set(
            [DataValue::Num(Num::Int(1)), DataValue::Num(Num::Int(2))]
                .into_iter()
                .collect(),
        ),
        // Vectors: negative values (raw-bit ordering breaks here), length
        // before content, both element widths.
        DataValue::Vec(Vector::F32(ndarray::arr1(&[-2.5f32, 0.0, 1.0]))),
        DataValue::Vec(Vector::F32(ndarray::arr1(&[-2.5f32, 0.5, 1.0]))),
        DataValue::Vec(Vector::F32(ndarray::arr1(&[1.0f32]))),
        DataValue::Vec(Vector::F32(ndarray::arr1(&[f32::NAN]))),
        DataValue::Vec(Vector::F64(ndarray::arr1(&[-7.5f64]))),
        DataValue::Vec(Vector::F64(ndarray::arr1(&[0.25f64, -7.5]))),
        DataValue::Json(JsonData(serde_json::json!({"a": 1}))),
        DataValue::Json(JsonData(serde_json::json!([1, 2, 3]))),
        DataValue::Validity(Validity {
            timestamp: ValidityTs(Reverse(42)),
            is_assert: Reverse(true),
        }),
        DataValue::Validity(Validity {
            timestamp: ValidityTs(Reverse(42)),
            is_assert: Reverse(false),
        }),
        DataValue::Validity(Validity {
            timestamp: ValidityTs(Reverse(41)),
            is_assert: Reverse(true),
        }),
        DataValue::Bot,
    ];
    // Nested collections — bound by name so corpus insertions can't silently
    // change which values get nested.
    let nested_set = DataValue::Set(
        [DataValue::Num(Num::Int(1)), DataValue::Num(Num::Int(2))]
            .into_iter()
            .collect(),
    );
    let nested_list = DataValue::List(vec![DataValue::Num(Num::Int(1))]);
    c.push(DataValue::List(vec![nested_set, nested_list]));
    c
}

fn encode(v: &DataValue) -> Vec<u8> {
    let mut buf = vec![];
    buf.encode_datavalue(v);
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
/// nobody thought to put in a corpus. Regex is excluded (arbitrary strings
/// are not valid patterns); the corpus covers it.
fn arb_value() -> impl Strategy<Value = DataValue> {
    let leaf = prop_oneof![
        Just(DataValue::Null),
        any::<bool>().prop_map(DataValue::Bool),
        any::<i64>().prop_map(|i| DataValue::Num(Num::Int(i))),
        any::<f64>().prop_map(|f| DataValue::Num(Num::Float(f))),
        "[\\PC]{0,12}".prop_map(|s| DataValue::Str(s.into())),
        proptest::collection::vec(any::<u8>(), 0..24).prop_map(DataValue::Bytes),
        any::<u128>().prop_map(|u| DataValue::Uuid(crate::UuidWrapper(uuid::Uuid::from_u128(u)))),
        proptest::collection::vec(any::<f32>(), 0..6)
            .prop_map(|v| DataValue::Vec(Vector::F32(ndarray::Array1::from(v)))),
        proptest::collection::vec(any::<f64>(), 0..6)
            .prop_map(|v| DataValue::Vec(Vector::F64(ndarray::Array1::from(v)))),
        (any::<i64>(), any::<bool>()).prop_map(|(ts, a)| DataValue::Validity(Validity {
            timestamp: ValidityTs(Reverse(ts)),
            is_assert: Reverse(a),
        })),
        Just(DataValue::Bot),
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
    let want: Vec<_> = model.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    assert_eq!(got, want, "store diverged from the model oracle");

    // Spot-check bounded scans against the model too.
    let got: Vec<_> = tx
        .range_scan(b"k005", b"k030")
        .map(|r| r.unwrap())
        .collect();
    let want: Vec<_> = model
        .range(b"k005".to_vec()..b"k030".to_vec())
        .map(|(k, v)| (k.clone(), v.clone()))
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
    assert_eq!(tx1.get(b"counter").unwrap(), Some(b"0".to_vec()));
    assert_eq!(tx2.get(b"counter").unwrap(), Some(b"0".to_vec()));
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
        Some(b"1".to_vec()),
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
    assert_eq!(w.get(b"x").unwrap(), Some(b"1".to_vec()), "RYOW");
    assert!(w.exists(b"x").unwrap());
    w.commit().unwrap();

    // A snapshot opened before the write never sees it.
    assert_eq!(reader_before.get(b"x").unwrap(), None, "snapshot isolation");
    // A snapshot opened after does.
    let reader_after = db.read_tx().unwrap();
    assert_eq!(reader_after.get(b"x").unwrap(), Some(b"1".to_vec()));
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
    assert_eq!(tx.get(b"z-outside").unwrap(), Some(b"stays".to_vec()));
}

// ---------- time travel: seek-based scan vs naive oracle ----------

fn vld_key(rel: RelationId, name: &str, ts: i64, assert: bool) -> EncodedKey {
    let tuple: Tuple = vec![
        DataValue::Str(name.into()),
        DataValue::Validity(Validity {
            timestamp: ValidityTs(Reverse(ts)),
            is_assert: Reverse(assert),
        }),
    ];
    tuple.encode_as_key(rel)
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
    let rel = RelationId::new(7);
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let mut tx = db.write_tx().unwrap();
    for (name, ts, assert) in history {
        tx.put(&vld_key(rel, name, *ts, *assert), b"").unwrap();
    }
    tx.commit().unwrap();

    let lower = rel.raw_encode().to_vec();
    let upper = rel.next().raw_encode().to_vec();
    let tx = db.read_tx().unwrap();
    for at in 0..=10i64 {
        let got: Vec<(String, i64)> = tx
            .range_skip_scan_tuple(&lower, &upper, ValidityTs(Reverse(at)))
            .map(|r| {
                let t = r.unwrap();
                let name = match &t[0] {
                    DataValue::Str(s) => s.to_string(),
                    v => panic!("unexpected {v:?}"),
                };
                let ts = match &t[1] {
                    DataValue::Validity(v) => v.timestamp.0.0,
                    v => panic!("unexpected {v:?}"),
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
    let rel = RelationId::new(7);
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    {
        let mut tx = db.write_tx().unwrap();
        tx.put(&vld_key(rel, "a", 1, true), b"").unwrap();
        tx.commit().unwrap();
    }
    let mut tx = db.write_tx().unwrap();
    tx.put(&vld_key(rel, "b", 2, true), b"").unwrap();
    let lower = rel.raw_encode().to_vec();
    let upper = rel.next().raw_encode().to_vec();
    let got: Vec<String> = tx
        .range_skip_scan_tuple(&lower, &upper, ValidityTs(Reverse(5)))
        .map(|r| match &r.unwrap()[0] {
            DataValue::Str(s) => s.to_string(),
            v => panic!("unexpected {v:?}"),
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

// ---------- sentinel and corruption edge cases ----------

/// A stored retraction at ts == i64::MIN collides with the TERMINAL_VALIDITY
/// seek sentinel; the scan must terminate (and skip it) rather than livelock.
#[test]
fn skip_scan_terminates_on_retraction_at_min_ts() {
    let rel = RelationId::new(7);
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let mut tx = db.write_tx().unwrap();
    tx.put(&vld_key(rel, "a", 1, true), b"").unwrap();
    tx.put(&vld_key(rel, "z", i64::MIN, false), b"").unwrap();
    tx.commit().unwrap();

    let tx = db.read_tx().unwrap();
    let got: Vec<String> = tx
        .range_skip_scan_tuple(
            rel.raw_encode().as_ref(),
            rel.next().raw_encode().as_ref(),
            ValidityTs(Reverse(5)),
        )
        .map(|r| match &r.unwrap()[0] {
            DataValue::Str(s) => s.to_string(),
            v => panic!("unexpected {v:?}"),
        })
        .collect();
    assert_eq!(got, vec!["a".to_string()]);
}

/// An assertion at ts == i64::MIN is a legitimate hit and must also terminate.
#[test]
fn skip_scan_hit_at_min_ts_terminates() {
    let rel = RelationId::new(7);
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let mut tx = db.write_tx().unwrap();
    tx.put(&vld_key(rel, "a", i64::MIN, true), b"").unwrap();
    tx.commit().unwrap();

    let tx = db.read_tx().unwrap();
    let got: Vec<Tuple> = tx
        .range_skip_scan_tuple(
            rel.raw_encode().as_ref(),
            rel.next().raw_encode().as_ref(),
            ValidityTs(Reverse(0)),
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
    enc.encode_datavalue(&DataValue::Validity(Validity {
        timestamp: ValidityTs(Reverse(1)),
        is_assert: Reverse(true),
    }));
    k.extend(enc);
    let _ = crate::data::tuple::check_key_for_validity(&k, ValidityTs(Reverse(5)), None);

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
        assert_eq!(tx.get(b"k").unwrap(), Some(b"v".to_vec()));
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
        Some(b"survives".to_vec()),
        "committed (unsynced) data must survive a process crash"
    );
    assert_eq!(
        tx.get(b"synced").unwrap(),
        Some(b"survives-power-cut-too".to_vec()),
        "synced data must survive a process crash"
    );
    assert_eq!(
        tx.get(b"durable").unwrap(),
        Some(b"per-tx-fsync".to_vec()),
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
    assert_eq!(tx.get(b"a").unwrap(), Some(b"1".to_vec()));
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
    assert_eq!(tx.get(b"k00039999").unwrap(), Some(b"v39999".to_vec()));
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
                        let cur: u64 = String::from_utf8(tx.get(b"counter").unwrap().unwrap())
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
    let total: u64 = String::from_utf8(tx.get(b"counter").unwrap().unwrap())
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
    use crate::data::tuple::extend_tuple_from_v;
    // rmp payload: array[1] { map{ "Regex": "" } } behind the 8-byte header.
    let mut hostile = vec![0u8; 8];
    hostile.extend([0x91, 0x81, 0xa5]);
    hostile.extend(b"Regex");
    hostile.push(0xa0);
    let mut tup: Tuple = vec![];
    assert!(extend_tuple_from_v(&mut tup, &hostile).is_err());
}

proptest! {
    /// Generative value-side Law 3: arbitrary value bytes never panic.
    #[test]
    fn law3_value_generative(bytes in proptest::collection::vec(any::<u8>(), 0..64)) {
        let mut tup: Tuple = vec![];
        let _ = crate::data::tuple::extend_tuple_from_v(&mut tup, &bytes);
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
    let mut bytes = b"KYZODMP1".to_vec();
    bytes.extend((1u64).to_be_bytes());
    bytes.extend(b"1");
    bytes.extend(u64::MAX.to_be_bytes());
    std::fs::write(&evil, &bytes).unwrap();
    let victim = new_fjall_storage(dir.path().join("victim")).unwrap();
    assert!(restore_storage(&victim, &evil).is_err());

    // Truncated mid-pair: error.
    let cut = dir.path().join("cut.kyzo");
    let mut bytes = b"KYZODMP1".to_vec();
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
        err.downcast_ref::<ConflictError>().is_some(),
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
    use crate::data::tuple::encode_tuple_key;
    use crate::storage::verify::verify_storage;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    {
        let db = new_fjall_storage(&path).unwrap();
        let mut tx = db.write_tx().unwrap();
        for i in 0..50i64 {
            let k = encode_tuple_key(7, &[DataValue::Num(Num::Int(i))]);
            tx.put(&k, b"").unwrap();
        }
        tx.commit().unwrap();
        let report = verify_storage(&db).unwrap();
        assert!(
            report.is_clean(),
            "healthy store must verify clean: {report:?}"
        );
        assert_eq!(report.checked, 50);
    }
    // Inject garbage below the kernel: raw fjall write of an undecodable key.
    {
        let raw = fjall::OptimisticTxDatabase::builder(&path).open().unwrap();
        let ks = raw
            .keyspace("kyzo", fjall::KeyspaceCreateOptions::default)
            .unwrap();
        ks.insert([0u8, 0, 0, 0, 0, 0, 0, 7, 0xEE, 0xEE], b"?")
            .unwrap();
        raw.persist(fjall::PersistMode::SyncAll).unwrap();
    }
    let db = new_fjall_storage(&path).unwrap();
    let report = verify_storage(&db).unwrap();
    assert!(!report.is_clean());
    assert_eq!(report.checked, 51, "the walk must continue past corruption");
    assert_eq!(report.corrupt.len(), 1);
    assert!(report.corrupt[0].error.contains("unknown type tag"));
}

/// Law 6 (concurrency liveness) through the retry helper: contended
/// read-modify-write across threads completes exactly, with conflicts
/// retried rather than surfaced.
#[test]
fn retry_on_conflict_reaches_completion_under_contention() {
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
                    retry_on_conflict(1_000, || {
                        let mut tx = db.write_tx()?;
                        let cur: u64 = String::from_utf8(tx.get(b"n")?.unwrap())
                            .unwrap()
                            .parse()
                            .unwrap();
                        tx.put(b"n", (cur + 1).to_string().as_bytes())?;
                        tx.commit()
                    })
                    .unwrap();
                }
            });
        }
    });
    let tx = db.read_tx().unwrap();
    let total: u64 = String::from_utf8(tx.get(b"n").unwrap().unwrap())
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
    assert_eq!(FormatVersion::parse(b"1").unwrap(), FormatVersion::CURRENT);
    for bad in [&b"01"[..], b"+1", b" 1", b"1 ", b""] {
        assert!(
            FormatVersion::parse(bad).is_err(),
            "must reject non-canonical stamp {bad:?}"
        );
    }
}
