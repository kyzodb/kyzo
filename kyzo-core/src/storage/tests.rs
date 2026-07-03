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
        tx.range_skip_scan_tuple(b"z", b"a", ValidityTs(Reverse(0)))
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
        Some(b"2".to_vec()),
        "inverted del_range must delete nothing"
    );
    assert_eq!(r.get(b"mine").unwrap(), Some(b"x".to_vec()));
    assert_eq!(r.get(b"after").unwrap(), Some(b"ok".to_vec()));
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
        err.downcast_ref::<ConflictError>().is_some(),
        "the write-write abort must be the typed, retryable conflict: {err:?}"
    );
    assert_eq!(
        db.read_tx().unwrap().get(b"ww").unwrap(),
        Some(b"1".to_vec()),
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
        Some(b"2".to_vec())
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
    assert!(err.downcast_ref::<ConflictError>().is_some());
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
    assert_eq!(ro.get(b"ww").unwrap(), Some(b"3".to_vec()));
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

// ---------- deterministic simulation (DST) at the storage seam ----------
//
// SimStorage (storage/sim.rs) is the contract's own test double: schedules,
// faults, crashes, and power cuts are all pure functions of one seed. A
// failing campaign panics with "FAILING SEED = n"; rerunning with that seed
// replays the schedule and fault plan exactly.

use std::collections::BTreeSet;

use crate::storage::retry::retry_on_conflict;
use crate::storage::sim::{
    FaultConfig, SimRng, SimStorage, TxBody, for_each_seed, run_interleaved,
};

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
    let want: Vec<_> = model.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    assert_eq!(got, want, "sim diverged from the model oracle");
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
    assert_eq!(tx1.get(b"counter").unwrap(), Some(b"0".to_vec()));
    assert_eq!(tx2.get(b"counter").unwrap(), Some(b"0".to_vec()));
    tx1.put(b"counter", b"1").unwrap();
    tx2.put(b"counter", b"2").unwrap();
    tx1.commit().unwrap();
    let err = tx2.commit().unwrap_err();
    assert!(
        err.downcast_ref::<ConflictError>().is_some(),
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
        err.downcast_ref::<ConflictError>().is_some(),
        "the write-write abort must be the typed conflict, got {err:?}"
    );
    assert_eq!(
        db.read_tx().unwrap().get(b"ww").unwrap(),
        Some(b"1".to_vec()),
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
            .downcast_ref::<ConflictError>()
            .is_some(),
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
    assert_eq!(ro.get(b"ww").unwrap(), Some(b"2".to_vec()));
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
    assert_eq!(w.get(b"x").unwrap(), Some(b"1".to_vec()), "RYOW");
    assert!(w.exists(b"x").unwrap());
    w.commit().unwrap();
    assert_eq!(reader_before.get(b"x").unwrap(), None, "snapshot isolation");
    assert_eq!(
        db.read_tx().unwrap().get(b"x").unwrap(),
        Some(b"1".to_vec())
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
    assert_eq!(tx.get(b"z-outside").unwrap(), Some(b"stays".to_vec()));
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
        err.downcast_ref::<ConflictError>().is_some(),
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
    let rel = RelationId::new(7);
    let db = SimStorage::new(2);
    let mut tx = db.write_tx().unwrap();
    for (name, ts, assert) in history {
        tx.put(&vld_key(rel, name, *ts, *assert), b"").unwrap();
    }
    // Own writes are visible to the write transaction's as-of scan.
    let lower = rel.raw_encode().to_vec();
    let upper = rel.next().raw_encode().to_vec();
    assert!(
        tx.range_skip_scan_tuple(&lower, &upper, ValidityTs(Reverse(1)))
            .next()
            .is_some(),
        "as-of scan must see own writes"
    );
    tx.commit().unwrap();

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
        assert_eq!(
            got,
            as_of_oracle(history, at),
            "as-of {at}: sim seek scan diverged from the naive oracle"
        );
    }
    // The sentinel edge: a retraction at ts == i64::MIN must not livelock.
    let db = SimStorage::new(3);
    let mut tx = db.write_tx().unwrap();
    tx.put(&vld_key(rel, "a", 1, true), b"").unwrap();
    tx.put(&vld_key(rel, "z", i64::MIN, false), b"").unwrap();
    tx.commit().unwrap();
    let tx = db.read_tx().unwrap();
    let got = tx
        .range_skip_scan_tuple(&lower, &upper, ValidityTs(Reverse(5)))
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
    assert_eq!(tx.get(b"k00002999").unwrap(), Some(b"v2999".to_vec()));

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
                    assert_eq!(v, Some(b"v".to_vec()), "a non-faulted read must be correct");
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
                        retry_on_conflict(10_000, || {
                            let mut tx = db.write_tx()?;
                            let mut log = tx.get(b"log")?.unwrap();
                            log.push(b'0' + id);
                            tx.put(b"log", &log)?;
                            tx.commit()
                        })
                        .unwrap();
                    }
                }) as TxBody<'_>
            })
            .collect();
        run_interleaved(&db, seed, bodies);
        db.read_tx().unwrap().get(b"log").unwrap().unwrap()
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
        retry_on_conflict(10_000, || {
            let mut tx = db.write_tx()?;
            tx.put(b"counter", b"0")?;
            tx.commit()
        })
        .unwrap();

        let bodies: Vec<TxBody<'_>> = (0..BODIES)
            .map(|b| {
                let db = db.clone();
                Box::new(move || {
                    for i in 0..OPS {
                        retry_on_conflict(10_000, || {
                            let mut tx = db.write_tx()?;
                            let cur: u64 = String::from_utf8(tx.get(b"counter")?.unwrap())
                                .unwrap()
                                .parse()
                                .unwrap();
                            tx.put(b"counter", (cur + 1).to_string().as_bytes())?;
                            tx.put(format!("b{b}-k{i}").as_bytes(), b"x")?;
                            tx.commit()
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
        let want: Vec<_> = model.into_iter().collect();
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
            let want: Vec<_> = model.into_iter().collect();
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
        let want_all: Vec<_> = all.into_iter().collect();
        assert_eq!(crash, want_all, "crash must keep every commit");
        let cut: Vec<_> = db
            .sim_powercut()
            .read_tx()
            .unwrap()
            .total_scan()
            .map(|r| r.unwrap())
            .collect();
        let want_synced: Vec<_> = synced.into_iter().collect();
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
    let rel = RelationId::new(9);
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
                        retry_on_conflict(10_000, || {
                            let mut tx = db.write_tx()?;
                            tx.put(&vld_key(rel, &name, ts, a), b"")?;
                            tx.commit()
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

    let parse = |v: Option<Vec<u8>>| -> u64 {
        String::from_utf8(v.expect("key must exist"))
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
                            e.downcast_ref::<ConflictError>().is_some(),
                            "only the typed conflict is a legal abort: {e:?}"
                        );
                        aborts.fetch_add(1, Ordering::Relaxed);
                        retry_on_conflict(10_000, || {
                            let mut tx = db.write_tx()?;
                            let n = parse(tx.get(src)?);
                            tx.put(dst, (n + 1).to_string().as_bytes())?;
                            tx.commit()
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
                    retry_on_conflict(10_000, || {
                        let mut tx = db.write_tx()?;
                        let n = tx.range_scan(b"p", b"q").count();
                        tx.put(b"summary", n.to_string().as_bytes())?;
                        tx.commit()
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
        let summary: usize = String::from_utf8(r.get(b"summary").unwrap().unwrap())
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
                            e.downcast_ref::<ConflictError>().is_some(),
                            "only the typed conflict is a legal abort: {e:?}"
                        );
                        aborts.fetch_add(1, Ordering::Relaxed);
                        retry_on_conflict(10_000, || {
                            let mut tx = db.write_tx()?;
                            tx.put(b"hot", val)?;
                            tx.commit()
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
        retry_on_conflict(10_000, || {
            let mut tx = db.write_tx()?;
            for i in 0..KEYS {
                tx.put(format!("r{i:02}").as_bytes(), b"v")?;
            }
            tx.commit()
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
            retry_on_conflict(1_000, || {
                let mut tx = db.write_tx()?;
                tx.put(format!("k{i}").as_bytes(), b"v")?;
                tx.commit()
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
