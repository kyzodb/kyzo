/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Story #77's interop cross-check: a REAL Arrow reader (the `arrow` crate,
//! never a dependency of kyzo-core) decodes what kyzo-core's own
//! dependency-free `NamedRows::to_arrow_ipc` encoder writes. kyzo-core's
//! own test suite proves the byte layout against the spec in isolation;
//! this proves an actual Arrow implementation agrees.

use std::io::Cursor;

use arrow::array::{Array, BinaryArray, BooleanArray, Float64Array, Int64Array, StringArray};
use arrow::ipc::reader::StreamReader;
use kyzo::{DataValue, GermanStr, NamedRows, Num};

fn v_int(i: i64) -> DataValue {
    DataValue::Num(Num::Int(i))
}
fn v_float(f: f64) -> DataValue {
    DataValue::Num(Num::Float(f))
}

/// Every uniformly-typed `ColumnVec` kind this encoder maps: Int64,
/// Float64, Bool, Utf8 — a real Arrow reader must recover exactly the
/// same headers, types, and values it was handed.
#[test]
fn real_arrow_reader_decodes_a_uniformly_typed_batch() {
    let named = NamedRows::new(
        vec!["n".into(), "x".into(), "flag".into(), "name".into()],
        vec![
            vec![
                v_int(1),
                v_float(1.5),
                DataValue::Bool(true),
                DataValue::Str("ab".into()),
            ]
            .into(),
            vec![
                v_int(2),
                v_float(2.5),
                DataValue::Bool(false),
                DataValue::Str("".into()),
            ]
            .into(),
            vec![
                v_int(3),
                v_float(3.5),
                DataValue::Bool(true),
                DataValue::Str("cde".into()),
            ]
            .into(),
        ],
    );
    let bytes = named.to_arrow_ipc().expect("encodes");

    let mut reader = StreamReader::try_new(Cursor::new(bytes), None).expect("valid Arrow stream");
    let schema = reader.schema();
    let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
    assert_eq!(names, vec!["n", "x", "flag", "name"]);

    let batch = reader.next().expect("one record batch").expect("readable");
    assert_eq!(batch.num_rows(), 3);
    assert!(reader.next().is_none(), "exactly one RecordBatch message");

    let n = batch
        .column(0)
        .as_any()
        .downcast_ref::<Int64Array>()
        .unwrap();
    assert_eq!(n.values(), &[1, 2, 3]);
    assert_eq!(n.null_count(), 0);

    let x = batch
        .column(1)
        .as_any()
        .downcast_ref::<Float64Array>()
        .unwrap();
    assert_eq!(x.values(), &[1.5, 2.5, 3.5]);

    let flag = batch
        .column(2)
        .as_any()
        .downcast_ref::<BooleanArray>()
        .unwrap();
    assert_eq!(
        flag.iter().collect::<Vec<_>>(),
        vec![Some(true), Some(false), Some(true)]
    );

    let name = batch
        .column(3)
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    assert_eq!(
        name.iter().collect::<Vec<_>>(),
        vec![Some("ab"), Some(""), Some("cde")]
    );
}

/// The nullable path: a `Mixed` column of `Null` + one concrete kind must
/// decode with a real validity bitmap a real Arrow reader honors.
#[test]
fn real_arrow_reader_decodes_nulls_via_the_validity_bitmap() {
    let named = NamedRows::new(
        vec!["n".into()],
        vec![
            vec![v_int(10)].into(),
            vec![DataValue::Null].into(),
            vec![v_int(30)].into(),
            vec![DataValue::Null].into(),
        ],
    );
    let bytes = named.to_arrow_ipc().expect("encodes");

    let mut reader = StreamReader::try_new(Cursor::new(bytes), None).expect("valid Arrow stream");
    let batch = reader.next().expect("one record batch").expect("readable");
    let n = batch
        .column(0)
        .as_any()
        .downcast_ref::<Int64Array>()
        .unwrap();
    assert_eq!(n.null_count(), 2);
    assert_eq!(
        n.iter().collect::<Vec<_>>(),
        vec![Some(10), None, Some(30), None]
    );
}

/// An empty batch (zero rows) is still a legal stream: schema, zero-row
/// RecordBatch, EOS — the shape KyzoDB produces for an empty result set.
#[test]
fn real_arrow_reader_decodes_a_zero_row_batch() {
    let named = NamedRows::new(vec!["n".into()], vec![]);
    let bytes = named.to_arrow_ipc().expect("encodes");

    let mut reader = StreamReader::try_new(Cursor::new(bytes), None).expect("valid Arrow stream");
    let batch = reader.next().expect("one record batch").expect("readable");
    assert_eq!(batch.num_rows(), 0);
}

/// `DataValue::Bytes` has no dedicated `ColumnVec` variant — a bytes
/// column always arrives as `Mixed`, classified `Kind::Binary` — so this
/// is the only path that exercises the encoder's Arrow `Binary` type at
/// all; a real reader must recover the raw bytes unchanged.
#[test]
fn real_arrow_reader_decodes_a_binary_column() {
    let named = NamedRows::new(
        vec!["blob".into()],
        vec![
            vec![DataValue::Bytes(GermanStr::from_bytes(&[0, 1, 2]))].into(),
            vec![DataValue::Bytes(GermanStr::from_bytes(&[]))].into(),
            vec![DataValue::Bytes(GermanStr::from_bytes(&[255, 254]))].into(),
        ],
    );
    let bytes = named.to_arrow_ipc().expect("encodes");

    let mut reader = StreamReader::try_new(Cursor::new(bytes), None).expect("valid Arrow stream");
    let batch = reader.next().expect("one record batch").expect("readable");
    let blob = batch
        .column(0)
        .as_any()
        .downcast_ref::<BinaryArray>()
        .unwrap();
    assert_eq!(blob.value(0), &[0, 1, 2]);
    assert_eq!(blob.value(1), &[] as &[u8]);
    assert_eq!(blob.value(2), &[255, 254]);
}
