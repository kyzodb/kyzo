/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use miette::{Result, bail, miette};

use crate::data::tuple::Tuple;
use crate::data::value::DataValue;

// ─────────────────────────────────────────────────────────────────────────
// The fact-value payload (FormatVersion 3): self-describing fields
// ─────────────────────────────────────────────────────────────────────────
//
// A fact's non-key columns are stored as a COUNT, an END-OFFSET TABLE,
// and TAGGED FIELDS — never as one monolithic serialized row. The layout
// exists for the columnar engine: field `i` of any row is one O(1) slice
// (`offsets[i-1]..offsets[i]`), and the scalar tags (int, float, bool)
// are fixed-width little-endian slots a batched gather can read straight
// into typed column vectors with no parser in the loop. Storage-layer
// iterators decode without catalog access, so the layout must carry its
// own shape — which the count + offsets + tags do.
//
//     [count: u16 LE]
//     [end_offset: u32 LE] × count      (relative to the fields region)
//     fields: [tag: u8][payload...] × count
//
// Tags: 0 null (empty), 1 bool (1 byte), 2 int (8 bytes LE),
// 3 float (8 bytes LE), 4 string (raw UTF-8), 5 other (msgpack island —
// lists, vectors, json, and every other composite, one field at a time).
//
// The measured cost of the shape (vs the previous monolithic msgpack
// row): narrow rows pay the count + offset table — one small int 12→15
// bytes, two 23→28, int/bool/int 30→34; bools and short strings break
// even. That overhead buys O(1) field slicing and parser-free scalar
// gather, which is the columnar engine's scan path; the losing shapes
// are published here deliberately.
// Decoding parses CLAIMED bytes: every length, offset, tag, and slot
// width is checked, and corruption is a typed error, never a panic.

const FIELD_NULL: u8 = 0;
const FIELD_BOOL: u8 = 1;
const FIELD_INT: u8 = 2;
const FIELD_FLOAT: u8 = 3;
const FIELD_STR: u8 = 4;
const FIELD_OTHER: u8 = 5;

/// Append the v3 payload for `values` to `out`.
pub(crate) fn encode_fact_payload(values: &[DataValue], out: &mut Vec<u8>) -> Result<()> {
    use serde::Serialize;
    let count = u16::try_from(values.len())
        .map_err(|_| miette!("row has more than 65535 non-key columns"))?;
    out.extend_from_slice(&count.to_le_bytes());
    let table_at = out.len();
    out.resize(table_at + 4 * values.len(), 0);
    let fields_at = out.len();
    for (i, v) in values.iter().enumerate() {
        match v {
            DataValue::Null => out.push(FIELD_NULL),
            DataValue::Bool(b) => {
                out.push(FIELD_BOOL);
                out.push(u8::from(*b));
            }
            DataValue::Num(crate::data::value::Num::Int(x)) => {
                out.push(FIELD_INT);
                out.extend_from_slice(&x.to_le_bytes());
            }
            DataValue::Num(crate::data::value::Num::Float(x)) => {
                out.push(FIELD_FLOAT);
                out.extend_from_slice(&x.to_le_bytes());
            }
            DataValue::Str(sv) => {
                out.push(FIELD_STR);
                out.extend_from_slice(sv.as_bytes());
            }
            other => {
                out.push(FIELD_OTHER);
                other
                    .serialize(&mut rmp_serde::Serializer::new(&mut *out))
                    .map_err(|e| miette!("cannot serialize row field: {e}"))?;
            }
        }
        let end = u32::try_from(out.len() - fields_at)
            .map_err(|_| miette!("row payload exceeds 4 GiB"))?;
        out[table_at + 4 * i..table_at + 4 * (i + 1)].copy_from_slice(&end.to_le_bytes());
    }
    Ok(())
}

/// Decode a v3 payload, extending `row` with its fields. Claimed bytes:
/// fallible everywhere.
pub(crate) fn decode_fact_payload(payload: &[u8], row: &mut Tuple) -> Result<()> {
    let count = usize::from(u16::from_le_bytes(
        payload
            .get(..2)
            .and_then(|b| b.try_into().ok())
            .ok_or_else(|| miette!("corrupt tuple value: missing field count"))?,
    ));
    let table = payload
        .get(2..2 + 4 * count)
        .ok_or_else(|| miette!("corrupt tuple value: truncated offset table"))?;
    let fields = payload
        .get(2 + 4 * count..)
        .ok_or_else(|| miette!("corrupt tuple value: missing fields region"))?;
    let mut start = 0usize;
    for i in 0..count {
        let end = u32::from_le_bytes(
            table
                .get(4 * i..4 * (i + 1))
                .and_then(|b| b.try_into().ok())
                .ok_or_else(|| miette!("corrupt tuple value: truncated offset table"))?,
        ) as usize;
        let field = fields.get(start..end).ok_or_else(|| {
            miette!("corrupt tuple value: field {i} offsets [{start}, {end}) out of range")
        })?;
        row.push(decode_fact_field(field, i)?);
        start = end;
    }
    // Totality: the last end-offset must claim the fields region exactly.
    // Unclaimed trailing bytes are corruption (an offset-table bit flip
    // that GREW the last field would otherwise pass silently).
    if start != fields.len() {
        bail!(
            "corrupt tuple value: {} unclaimed trailing bytes",
            fields.len() - start
        );
    }
    Ok(())
}

fn decode_fact_field(field: &[u8], i: usize) -> Result<DataValue> {
    let (&tag, body) = field
        .split_first()
        .ok_or_else(|| miette!("corrupt tuple value: field {i} has no tag"))?;
    Ok(match tag {
        FIELD_NULL => {
            if !body.is_empty() {
                bail!("corrupt tuple value: null field {i} carries bytes");
            }
            DataValue::Null
        }
        FIELD_BOOL => match body {
            [0] => DataValue::Bool(false),
            [1] => DataValue::Bool(true),
            _ => bail!("corrupt tuple value: bool field {i} malformed"),
        },
        FIELD_INT => {
            DataValue::from(i64::from_le_bytes(body.try_into().map_err(|_| {
                miette!("corrupt tuple value: int field {i} not 8 bytes")
            })?))
        }
        FIELD_FLOAT => {
            DataValue::from(f64::from_le_bytes(body.try_into().map_err(|_| {
                miette!("corrupt tuple value: float field {i} not 8 bytes")
            })?))
        }
        FIELD_STR => DataValue::Str(
            std::str::from_utf8(body)
                .map_err(|_| miette!("corrupt tuple value: string field {i} not UTF-8"))?
                .into(),
        ),
        FIELD_OTHER => {
            let mut cursor = std::io::Cursor::new(body);
            let v: DataValue = rmp_serde::from_read(&mut cursor)
                .map_err(|e| miette!("corrupt tuple value: field {i}: {e}"))?;
            // The island must claim its whole body: bytes past the value
            // are corruption, not padding.
            if cursor.position() != body.len() as u64 {
                bail!("corrupt tuple value: field {i} carries trailing bytes");
            }
            v
        }
        other => bail!("corrupt tuple value: unknown field tag {other:#04x} at field {i}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The v3 payload law: every value class round-trips exactly, and
    /// every corruption shape refuses with a typed error, never a panic.
    #[test]
    fn fact_payload_round_trips_and_refuses_corruption() {
        let all_classes: Tuple = vec![
            DataValue::Null,
            DataValue::Bool(true),
            DataValue::from(-42i64),
            DataValue::from(1.5f64),
            DataValue::from("héllo"),
            DataValue::List(vec![DataValue::from(1), DataValue::from("x")]),
        ];
        let mut payload = vec![];
        encode_fact_payload(&all_classes, &mut payload).unwrap();
        let mut out: Tuple = vec![];
        decode_fact_payload(&payload, &mut out).unwrap();
        assert_eq!(out, all_classes, "every class round-trips");

        // Empty row: two count bytes, nothing else.
        let mut empty = vec![];
        encode_fact_payload(&[], &mut empty).unwrap();
        assert_eq!(empty, vec![0, 0]);
        let mut out: Tuple = vec![];
        decode_fact_payload(&empty, &mut out).unwrap();
        assert!(out.is_empty());

        // Scalar fields are FIXED-WIDTH SLOTS at O(1) offsets — the
        // columnar contract. Field 2 (-42i64) of the payload above:
        // fields region starts at 2 + 4*6; its start is field 1's end.
        let table_at = 2;
        let fields_at = table_at + 4 * 6;
        let end_of = |i: usize| -> usize {
            u32::from_le_bytes(
                payload[table_at + 4 * i..table_at + 4 * (i + 1)]
                    .try_into()
                    .unwrap(),
            ) as usize
        };
        let f2 = &payload[fields_at + end_of(1)..fields_at + end_of(2)];
        assert_eq!(f2[0], FIELD_INT);
        assert_eq!(i64::from_le_bytes(f2[1..9].try_into().unwrap()), -42);

        // Corruption shapes: typed refusals, never panics.
        let corrupt: Vec<Vec<u8>> = vec![
            vec![1],                // truncated count
            vec![2, 0, 1, 0, 0, 0], // table shorter than count
            {
                // offset beyond the fields region
                let mut c = vec![1, 0, 9, 9, 0, 0];
                c.push(FIELD_INT);
                c.extend(0i64.to_le_bytes());
                c
            },
            vec![1, 0, 1, 0, 0, 0, 0xEE],                  // unknown tag
            vec![1, 0, 2, 0, 0, 0, FIELD_BOOL, 7],         // malformed bool
            vec![1, 0, 5, 0, 0, 0, FIELD_INT, 1, 2, 3, 4], // short int slot
            vec![1, 0, 3, 0, 0, 0, FIELD_STR, 0xFF, 0xFE], // non-UTF8 string
            vec![1, 0, 2, 0, 0, 0, FIELD_NULL, 9],         // null with bytes
            // unclaimed trailing bytes after a valid field
            vec![1, 0, 1, 0, 0, 0, FIELD_NULL, 0xAA, 0xBB, 0xCC],
        ];
        for (i, bytes) in corrupt.iter().enumerate() {
            let mut out: Tuple = vec![];
            assert!(
                decode_fact_payload(bytes, &mut out).is_err(),
                "corruption shape {i} must refuse: {bytes:?}"
            );
        }

        // An msgpack island with trailing bytes refuses too: encode a
        // list field, then grow its recorded extent by appending garbage.
        let mut payload = vec![];
        encode_fact_payload(&[DataValue::List(vec![DataValue::from(1)])], &mut payload).unwrap();
        payload.extend_from_slice(&[0xDE, 0xAD]);
        let grown = (u32::from_le_bytes(payload[2..6].try_into().unwrap()) + 2).to_le_bytes();
        payload[2..6].copy_from_slice(&grown);
        let mut out: Tuple = vec![];
        assert!(
            decode_fact_payload(&payload, &mut out).is_err(),
            "island trailing bytes must refuse"
        );
    }
}
