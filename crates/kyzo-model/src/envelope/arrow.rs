/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! A dependency-free encoder for the Arrow IPC STREAMING format (one Schema
//! message, then any number of RecordBatch messages, then an end-of-stream
//! marker), built directly against [`ColumnBatch`] — never against the
//! `arrow` crate. The `arrow` crate is not (and, per the pure-Rust
//! invariant, cannot be) a dependency of this crate: `arrow-array`
//! unconditionally requires `chrono`'s `clock` feature, which pulls
//! `iana-time-zone`, which needs `core-foundation-sys` on macOS and
//! `windows-core` on Windows — the exact platform-native-binding class
//! this codebase migrated away from. Verified through `cargo tree -e
//! normal,build --target=all -i core-foundation-sys`: arrow's chrono
//! dependency is the pull-in path; note that Linux-only `cargo tree`
//! (without `--target=all`) hides this platform-specific path.
//!
//! The only new dependency is `flatbuffers`, the pure-Rust metadata-object
//! runtime library (used here as a plain buffer builder, no code
//! generation, no `flatc`) — verified dependency-free on every platform:
//! its own subtree is `{bitflags, rustc_version -> semver}`, no `-sys`
//! crate anywhere.
//!
//! ## Scope
//!
//! Every [`ColumnVec`] variant KyzoDB's
//! batch machinery already decides between: `I64` -> Arrow `Int64`, `F64`
//! -> Arrow `Float64`, `Bool` -> Arrow `Bool` (bit-packed), `Str` -> Arrow
//! `Utf8`. None of these carries a null today (each variant's own doc says
//! "every element a non-null ..."), so their validity buffer is the
//! all-valid case — Arrow permits omitting it (zero-length buffer, `null_count
//! = 0`), which this encoder does. `Mixed` is supported too, but only when
//! every value is `Null` or one consistent concrete type among the four
//! above (a real, common shape: "an optional int column", say) — encoded
//! as that Arrow type WITH a genuine validity bitmap. A `Mixed` column
//! that is genuinely heterogeneous (more than one non-null kind, or a
//! kind with no Arrow mapping here: `List`/`Set`/`Vec`/`Json`/`Uuid`/
//! `Regex`/`Validity`/`Interval`/`Bytes`/`Bot`) is a typed refusal, never
//! a silent lossy re-encoding.
//!
//! ## Correctness
//!
//! The byte layout is checked two ways, on purpose kept apart: pure,
//! dependency-free round-trip/order tests live in this crate (below); the
//! claim that a REAL Arrow implementation can read this encoder's output
//! is proven in `kyzo-arrow-interop`, an isolated workspace member outside
//! the `kyzo`/`kyzo-bin` dependency trees the purity gate walks (see that
//! crate's `Cargo.toml`) — it depends on the real `arrow` crate and does
//! nothing but decode what this module writes.

use flatbuffers::{FlatBufferBuilder, UOffsetT, WIPOffset};
use miette::{Result, bail};

use crate::data_value_any;
use crate::value::{DataValue, NumRepr, Tuple};

/// One export column, decided once from its values: a uniform typed
/// vector when every value fits one of the four Arrow-mappable kinds,
/// `Mixed` otherwise (including any `Null`, which forces the nullable
/// path). These are the ENCODER'S OWN planning types — the export
/// boundary's vocabulary, not an execution currency.
pub enum ColumnVec {
    I64(Vec<i64>),
    F64(Vec<f64>),
    Bool(Vec<bool>),
    Str(Vec<String>),
    Mixed(Vec<DataValue>),
}

impl ColumnVec {
    fn from_values(values: Vec<DataValue>) -> ColumnVec {
        #[derive(PartialEq, Eq, Clone, Copy)]
        enum Fit {
            Int,
            Float,
            Bool,
            Str,
        }
        let mut fit: Option<Fit> = None;
        for v in &values {
            let this = match v {
                DataValue::Num(n) => match n.repr() {
                    NumRepr::Int(_) => Fit::Int,
                    NumRepr::Float(_) => Fit::Float,
                },
                DataValue::Bool(_) => Fit::Bool,
                DataValue::Str(_) => Fit::Str,
                data_value_any!() => return ColumnVec::Mixed(values),
            };
            match fit {
                None => fit = Some(this),
                Some(f) if f == this => {}
                Some(_) => return ColumnVec::Mixed(values),
            }
        }
        match fit {
            Some(Fit::Int) => ColumnVec::I64(
                values
                    .iter()
                    .map(|v| v.get_int().expect("uniform int column"))
                    .collect(),
            ),
            Some(Fit::Float) => ColumnVec::F64(
                values
                    .iter()
                    .map(|v| v.get_float().expect("uniform float column"))
                    .collect(),
            ),
            Some(Fit::Bool) => ColumnVec::Bool(
                values
                    .iter()
                    .map(|v| v.get_bool().expect("uniform bool column"))
                    .collect(),
            ),
            Some(Fit::Str) => ColumnVec::Str(
                values
                    .into_iter()
                    .map(|v| match v {
                        DataValue::Str(s) => s,
                        data_value_any!() => unreachable!("uniform str column"),
                    })
                    .collect(),
            ),
            None => ColumnVec::Mixed(values),
        }
    }
}

/// A row-set pivoted to columns for the export planner.
pub struct ColumnBatch {
    columns: Vec<ColumnVec>,
    height: usize,
}

/// Wrong-width row refused by [`ColumnBatch::try_from_rows`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColumnBatchWidthError {
    pub(crate) expected: usize,
    pub(crate) got: usize,
    pub(crate) row: usize,
}

impl std::fmt::Display for ColumnBatchWidthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "ColumnBatch row {} has width {}, expected {}",
            self.row, self.got, self.expected
        )
    }
}

impl std::error::Error for ColumnBatchWidthError {}

impl ColumnBatch {
    /// Refuse rows whose width is not exactly `arity` — never silently
    /// truncate with `.take(arity)`.
    pub fn try_from_rows(
        rows: Vec<Tuple>,
        arity: usize,
    ) -> std::result::Result<ColumnBatch, ColumnBatchWidthError> {
        let height = rows.len();
        let mut cols: Vec<Vec<DataValue>> =
            (0..arity).map(|_| Vec::with_capacity(height)).collect();
        for (row_i, row) in rows.into_iter().enumerate() {
            if row.len() != arity {
                return Err(ColumnBatchWidthError {
                    expected: arity,
                    got: row.len(),
                    row: row_i,
                });
            }
            for (i, v) in row.into_iter().enumerate() {
                cols[i].push(v);
            }
        }
        Ok(ColumnBatch {
            columns: cols.into_iter().map(ColumnVec::from_values).collect(),
            height,
        })
    }

    /// Convenience door for call sites that already prove row width.
    pub fn from_rows(rows: Vec<Tuple>, arity: usize) -> ColumnBatch {
        Self::try_from_rows(rows, arity)
            .expect("INVARIANT(column_batch_width): every row width equals arity")
    }

    pub fn width(&self) -> usize {
        self.columns.len()
    }

    pub fn height(&self) -> usize {
        self.height
    }

    fn column(&self, i: usize) -> &ColumnVec {
        &self.columns[i]
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Arrow's flatbuffer IDL constants (org.apache.arrow.flatbuf.*), hand-kept
// in sync with the format's Schema.fbs/Message.fbs — see the module doc
// for why these are hand-encoded rather than flatc-generated.
// ─────────────────────────────────────────────────────────────────────────

/// `org.apache.arrow.flatbuf.MetadataVersion.V5` — the current format
/// version as of Arrow 1.0 (unchanged since; V4 introduced the streaming
/// format this encoder targets).
const METADATA_VERSION_V5: i16 = 4;

/// `org.apache.arrow.flatbuf.MessageHeader` union discriminants.
const MESSAGE_HEADER_SCHEMA: u8 = 1;
const MESSAGE_HEADER_RECORD_BATCH: u8 = 3;

/// `org.apache.arrow.flatbuf.Type` union discriminants (the subset this
/// encoder emits).
const TYPE_INT: u8 = 2;
const TYPE_BINARY: u8 = 4;
const TYPE_UTF8: u8 = 5;
const TYPE_BOOL: u8 = 6;
const TYPE_FLOATING_POINT: u8 = 3;

/// `org.apache.arrow.flatbuf.Precision.DOUBLE` (`FloatingPoint.precision`).
const PRECISION_DOUBLE: i16 = 2;

/// `org.apache.arrow.flatbuf.Endianness.Little` (`Schema.endianness`).
const ENDIANNESS_LITTLE: i16 = 0;

/// The IPC streaming format's continuation marker: every message (and the
/// end-of-stream marker) starts with this after the format moved off the
/// legacy no-continuation encoding (Arrow format >= 0.15).
const CONTINUATION_MARKER: u32 = 0xFFFF_FFFF;

/// Round a length up to the next multiple of 8 — every flatbuffer message
/// and every body buffer is individually padded to this alignment; readers
/// rely on it to `mmap`/zero-copy the body without re-copying.
fn align8(len: usize) -> usize {
    len.div_ceil(8) * 8
}

// ─────────────────────────────────────────────────────────────────────────
// FieldNode / Buffer: flatbuffer STRUCTS (inline, no vtable) — two `long`
// (i64) fields each, 16 bytes, written little-endian per the format's
// fixed struct layout. The natural way to hand these to `flatbuffers` is
// `create_vector::<T>`, but that needs `T: Push`, whose method is `unsafe
// fn` — forbidden outright by this crate's `#![forbid(unsafe_code)]`
// (`crates/kyzo-core/src/lib.rs`), which does not carve out an exception for
// trait methods that happen to do nothing unsafe in the body. So these
// two vectors are built by [`push_struct_vector`] instead: every field of
// every element goes through the crate's own (safe) `i64: Push`, one
// scalar at a time, in the exact reverse order that makes the builder's
// backward-growing buffer come out forward-correct — the same technique
// this module already uses for the plain offset vector in
// `write_schema_message`.
// ─────────────────────────────────────────────────────────────────────────

/// Build a flatbuffer vector of 2-`i64`-field structs (`FieldNode{length,
/// null_count}` or `Buffer{offset, length}` — both the same physical
/// shape: two consecutive little-endian `i64`s, no padding) without ever
/// implementing `Push` (which would need `unsafe fn`, forbidden here).
/// `elements` gives each struct's `(first, second)` field pair in element
/// order. The two fields are plain scalars (a row count, a null count, a
/// byte offset, a byte length — never a flatbuffer reference), so they go
/// through the crate's own `i64: Push` unchanged, with no offset-relative
/// adjustment; the vector's own offset is what a table slot must run
/// through [`WIPOffset`]'s relative math, which is why this returns one.
fn push_struct_vector<'a>(
    fbb: &mut FlatBufferBuilder<'a>,
    elements: &[(i64, i64)],
) -> WIPOffset<UOffsetT> {
    fbb.start_vector::<i64>(elements.len() * 2);
    for &(first, second) in elements.iter().rev() {
        fbb.push::<i64>(second);
        fbb.push::<i64>(first);
    }
    WIPOffset::new(fbb.end_vector::<i64>(elements.len()).value())
}

// ─────────────────────────────────────────────────────────────────────────
// One Arrow-mappable column, decided once from a ColumnVec.
// ─────────────────────────────────────────────────────────────────────────

/// The physical buffers one column contributes to a RecordBatch's body,
/// plus enough type information to write its `Field`/`FieldNode`. Built by
/// [`plan_column`], consumed by [`write_schema_message`] (type only) and
/// [`write_record_batch_message`] (buffers).
/// Arrow field nullability — never a bare `bool` on [`PlannedColumn`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArrowNullability {
    Required,
    Optional,
}

impl ArrowNullability {
    fn is_optional(self) -> bool {
        matches!(self, ArrowNullability::Optional)
    }
}

#[derive(Debug)]
struct PlannedColumn {
    arrow_type: u8,
    nullability: ArrowNullability,
    null_count: i64,
    /// In per-buffer order (validity, [offsets], values) — omitted
    /// (zero-length) when there is nothing to say, e.g. an all-valid
    /// column's validity buffer.
    buffers: Vec<Vec<u8>>,
}

/// A validity bitmap: LSB-first, one bit per row, `1` = valid (Arrow's own
/// convention). Returns `None` (the omit-the-buffer case) when every row
/// is valid — `null_count` in the `FieldNode` is what actually tells a
/// reader whether to expect nulls; an all-valid column need not carry a
/// buffer of all-one bits at all, and Arrow explicitly permits this.
fn validity_bitmap(valid: &[bool]) -> (i64, Option<Vec<u8>>) {
    let null_count = valid.iter().filter(|v| !**v).count() as i64;
    if null_count == 0 {
        return (0, None);
    }
    let mut bytes = vec![0u8; valid.len().div_ceil(8)];
    for (i, v) in valid.iter().enumerate() {
        if *v {
            bytes[i / 8] |= 1 << (i % 8);
        }
    }
    (null_count, Some(bytes))
}

fn le_bytes_i64(values: &[i64]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}
fn le_bytes_f64(values: &[f64]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}
fn bool_bitpack(values: &[bool]) -> Vec<u8> {
    let mut bytes = vec![0u8; values.len().div_ceil(8)];
    for (i, v) in values.iter().enumerate() {
        if *v {
            bytes[i / 8] |= 1 << (i % 8);
        }
    }
    bytes
}
/// Arrow's variable-length layout: an `i32` offsets buffer (one more entry
/// than rows, `offsets[0] == 0`) plus the concatenated raw bytes.
/// Refuses when the cumulative byte length does not fit `i32`.
fn offsets_and_values<'a>(items: impl Iterator<Item = &'a [u8]>) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut offsets = Vec::new();
    let mut values = Vec::new();
    let mut cur: i32 = 0;
    offsets.extend_from_slice(&cur.to_le_bytes());
    for item in items {
        values.extend_from_slice(item);
        let len = i32::try_from(item.len())
            .map_err(|_| miette::miette!("Arrow offset: single value length exceeds i32::MAX"))?;
        cur = cur.checked_add(len).ok_or_else(|| {
            miette::miette!("Arrow offset: cumulative values length exceeds i32::MAX")
        })?;
        offsets.extend_from_slice(&cur.to_le_bytes());
    }
    Ok((offsets, values))
}

fn plan_column(col: &ColumnVec) -> Result<PlannedColumn> {
    match col {
        ColumnVec::I64(v) => Ok(PlannedColumn {
            arrow_type: TYPE_INT,
            nullability: ArrowNullability::Required,
            null_count: 0,
            buffers: vec![Vec::new(), le_bytes_i64(v)],
        }),
        ColumnVec::F64(v) => Ok(PlannedColumn {
            arrow_type: TYPE_FLOATING_POINT,
            nullability: ArrowNullability::Required,
            null_count: 0,
            buffers: vec![Vec::new(), le_bytes_f64(v)],
        }),
        ColumnVec::Bool(v) => Ok(PlannedColumn {
            arrow_type: TYPE_BOOL,
            nullability: ArrowNullability::Required,
            null_count: 0,
            buffers: vec![Vec::new(), bool_bitpack(v)],
        }),
        ColumnVec::Str(v) => {
            let (offsets, values) = offsets_and_values(v.iter().map(|s| s.as_bytes()))?;
            Ok(PlannedColumn {
                arrow_type: TYPE_UTF8,
                nullability: ArrowNullability::Required,
                null_count: 0,
                buffers: vec![Vec::new(), offsets, values],
            })
        }
        ColumnVec::Mixed(v) => plan_mixed_column(v),
    }
}

/// A `Mixed` column is Arrow-mappable only when every value is `Null` or
/// one consistent concrete type among the four this encoder knows — the
/// common "nullable typed column" shape `ColumnVec::from_values` cannot
/// represent directly (any `Null` makes it fall to `Mixed`, per that
/// function's own fit detection). Anything else is refused, named.
fn plan_mixed_column(values: &[DataValue]) -> Result<PlannedColumn> {
    #[derive(PartialEq, Eq, Clone, Copy)]
    enum Kind {
        Int,
        Float,
        Bool,
        Str,
        Binary,
    }
    let mut kind: Option<Kind> = None;
    for v in values {
        let this = match v {
            DataValue::Null => continue,
            DataValue::Num(n) => match n.repr() {
                NumRepr::Int(_) => Kind::Int,
                NumRepr::Float(_) => Kind::Float,
            },
            DataValue::Bool(_) => Kind::Bool,
            DataValue::Str(_) => Kind::Str,
            DataValue::Bytes(_) => Kind::Binary,
            other @ (data_value_any!()) => bail!(
                "Arrow export: column value {other:?} has no Arrow mapping in this encoder \
                 (supported: null, int, float, bool, str, bytes)"
            ),
        };
        match kind {
            None => kind = Some(this),
            Some(k) if k == this => {}
            Some(_) => bail!(
                "Arrow export: column mixes more than one non-null kind — Arrow columns are \
                 single-typed, this encoder does not fall back to a lossy re-encoding"
            ),
        }
    }
    let valid: Vec<bool> = values
        .iter()
        .map(|v| !matches!(v, DataValue::Null))
        .collect();
    let (null_count, validity) = validity_bitmap(&valid);
    let validity = match validity {
        Some(v) => v,
        None => Vec::new(),
    };
    match kind {
        None => {
            // Every value null: type is arbitrary (Arrow's own `Null` type
            // would be more precise, but this encoder's scope is the four
            // concrete kinds above) — pick Int64 as the total-null case.
            let zeros = vec![0i64; values.len()];
            Ok(PlannedColumn {
                arrow_type: TYPE_INT,
                nullability: ArrowNullability::Optional,
                null_count: values.len() as i64,
                buffers: vec![validity, le_bytes_i64(&zeros)],
            })
        }
        Some(Kind::Int) => {
            let vals: Vec<i64> = values
                .iter()
                .map(|v| match v.get_int() {
                    Some(n) => n,
                    None => 0,
                })
                .collect();
            Ok(PlannedColumn {
                arrow_type: TYPE_INT,
                nullability: ArrowNullability::Optional,
                null_count,
                buffers: vec![validity, le_bytes_i64(&vals)],
            })
        }
        Some(Kind::Float) => {
            let vals: Vec<f64> = values
                .iter()
                .map(|v| match v {
                    DataValue::Num(n) => match n.as_float() {
                        Some(f) => f,
                        None => 0.0,
                    },
                    data_value_any!() => 0.0,
                })
                .collect();
            Ok(PlannedColumn {
                arrow_type: TYPE_FLOATING_POINT,
                nullability: ArrowNullability::Optional,
                null_count,
                buffers: vec![validity, le_bytes_f64(&vals)],
            })
        }
        Some(Kind::Bool) => {
            let vals: Vec<bool> = values
                .iter()
                .map(|v| matches!(v, DataValue::Bool(true)))
                .collect();
            Ok(PlannedColumn {
                arrow_type: TYPE_BOOL,
                nullability: ArrowNullability::Optional,
                null_count,
                buffers: vec![validity, bool_bitpack(&vals)],
            })
        }
        Some(Kind::Str) => {
            let empty = String::new();
            let (offsets, data) = offsets_and_values(values.iter().map(|v| match v {
                DataValue::Str(s) => s.as_bytes(),
                data_value_any!() => empty.as_bytes(),
            }))?;
            Ok(PlannedColumn {
                arrow_type: TYPE_UTF8,
                nullability: ArrowNullability::Optional,
                null_count,
                buffers: vec![validity, offsets, data],
            })
        }
        Some(Kind::Binary) => {
            let empty: Vec<u8> = Vec::new();
            let (offsets, data) = offsets_and_values(values.iter().map(|v| match v {
                DataValue::Bytes(b) => b.as_slice(),
                data_value_any!() => empty.as_slice(),
            }))?;
            Ok(PlannedColumn {
                arrow_type: TYPE_BINARY,
                nullability: ArrowNullability::Optional,
                null_count,
                buffers: vec![validity, offsets, data],
            })
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Message framing
// ─────────────────────────────────────────────────────────────────────────

/// Wrap a finished flatbuffer `Message` in the IPC streaming envelope:
/// continuation marker, little-endian metadata length, the flatbuffer
/// bytes themselves padded so the body starts 8-byte aligned, then the
/// body.
fn frame_message(message_fb: &[u8], body: &[u8], out: &mut Vec<u8>) {
    let padded_len = align8(message_fb.len());
    out.extend_from_slice(&CONTINUATION_MARKER.to_le_bytes());
    out.extend_from_slice(&(padded_len as u32).to_le_bytes());
    out.extend_from_slice(message_fb);
    out.resize(out.len() + (padded_len - message_fb.len()), 0);
    out.extend_from_slice(body);
}

/// The end-of-stream marker: a continuation marker followed by a
/// zero-length metadata size, with no message and no body.
fn write_eos(out: &mut Vec<u8>) {
    out.extend_from_slice(&CONTINUATION_MARKER.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
}

/// Build one `Field` table (name, nullable, type_type, type). Returns the
/// finished offset.
///
/// Every reference field below (`type_`, `name`, and — in the callers —
/// `fields`/`nodes`/`buffers`/`header`) is pushed as a live [`WIPOffset`],
/// never as a bare [`UOffsetT`]: `WIPOffset<T>: Push` computes the
/// relative distance from the slot being written to the referenced
/// table/vector, which is what a flatbuffer reference actually is on the
/// wire — pushing the raw absolute position instead (this module's first
/// draft did, and the interop crate's reader caught it immediately as a
/// `SignedOffsetOutOfBounds`) produces a syntactically plausible but
/// unreadable stream.
fn build_field<'a>(
    fbb: &mut FlatBufferBuilder<'a>,
    name: &str,
    nullability: ArrowNullability,
    arrow_type: u8,
) -> WIPOffset<UOffsetT> {
    let type_offset: WIPOffset<UOffsetT> = match arrow_type {
        TYPE_INT => {
            let start = fbb.start_table();
            fbb.push_slot_always::<i32>(4, 64); // bitWidth
            fbb.push_slot_always::<bool>(6, true); // is_signed
            WIPOffset::new(fbb.end_table(start).value())
        }
        TYPE_FLOATING_POINT => {
            let start = fbb.start_table();
            fbb.push_slot_always::<i16>(4, PRECISION_DOUBLE); // precision
            WIPOffset::new(fbb.end_table(start).value())
        }
        TYPE_UTF8 | TYPE_BOOL | TYPE_BINARY => {
            // Empty tables (Utf8 {}, Bool {}, Binary {}): no fields to push.
            let start = fbb.start_table();
            WIPOffset::new(fbb.end_table(start).value())
        }
        _other => unreachable!("plan_column only ever produces the types matched above"),
    };
    let name_offset = fbb.create_string(name);
    let field_start = fbb.start_table();
    fbb.push_slot_always(4, name_offset); // name
    fbb.push_slot_always::<bool>(6, nullability.is_optional()); // nullable
    fbb.push_slot_always::<u8>(8, arrow_type); // type_type
    fbb.push_slot_always(10, type_offset); // type_
    // slots 12 (dictionary), 14 (children), 16 (custom_metadata) omitted —
    // all optional, and every field here is a leaf with no dictionary.
    WIPOffset::new(fbb.end_table(field_start).value())
}

/// Encode a Schema message: one `Field` per planned column, in order.
fn write_schema_message(fields: &[(&str, &PlannedColumn)]) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();
    let field_offsets: Vec<WIPOffset<UOffsetT>> = fields
        .iter()
        .map(|(name, col)| build_field(&mut fbb, name, col.nullability, col.arrow_type))
        .collect();
    fbb.start_vector::<UOffsetT>(field_offsets.len());
    for off in field_offsets.iter().rev() {
        fbb.push(*off);
    }
    let fields_vec = fbb.end_vector::<UOffsetT>(field_offsets.len());

    let schema_start = fbb.start_table();
    fbb.push_slot_always::<i16>(4, ENDIANNESS_LITTLE); // endianness
    fbb.push_slot_always(6, fields_vec); // fields
    let schema_offset = fbb.end_table(schema_start);

    let message_start = fbb.start_table();
    fbb.push_slot_always::<i16>(4, METADATA_VERSION_V5); // version
    fbb.push_slot_always::<u8>(6, MESSAGE_HEADER_SCHEMA); // header_type
    fbb.push_slot_always(8, schema_offset); // header
    fbb.push_slot_always::<i64>(10, 0); // bodyLength
    let message_offset = fbb.end_table(message_start);

    fbb.finish_minimal(message_offset);
    fbb.finished_data().to_vec()
}

/// Encode one RecordBatch message's metadata (nodes + buffer descriptors)
/// and return it alongside the concatenated, individually-padded body.
fn write_record_batch_message(height: usize, planned: &[PlannedColumn]) -> (Vec<u8>, Vec<u8>) {
    let mut body = Vec::new();
    let mut buffer_descs: Vec<(i64, i64)> = Vec::new();
    for col in planned {
        for buf in &col.buffers {
            let offset = body.len() as i64;
            let padded = align8(buf.len());
            buffer_descs.push((offset, buf.len() as i64)); // Buffer{offset, length}
            body.extend_from_slice(buf);
            body.resize(body.len() + (padded - buf.len()), 0);
        }
    }

    let mut fbb = FlatBufferBuilder::new();
    let nodes: Vec<(i64, i64)> = planned
        .iter()
        .map(|c| (height as i64, c.null_count)) // FieldNode{length, null_count}
        .collect();
    let nodes_vec = push_struct_vector(&mut fbb, &nodes);
    let buffers_vec = push_struct_vector(&mut fbb, &buffer_descs);

    let rb_start = fbb.start_table();
    fbb.push_slot_always::<i64>(4, height as i64); // length
    fbb.push_slot_always(6, nodes_vec); // nodes
    fbb.push_slot_always(8, buffers_vec); // buffers
    let rb_offset = fbb.end_table(rb_start);

    let message_start = fbb.start_table();
    fbb.push_slot_always::<i16>(4, METADATA_VERSION_V5); // version
    fbb.push_slot_always::<u8>(6, MESSAGE_HEADER_RECORD_BATCH); // header_type
    fbb.push_slot_always(8, rb_offset); // header
    fbb.push_slot_always::<i64>(10, body.len() as i64); // bodyLength
    let message_offset = fbb.end_table(message_start);

    fbb.finish_minimal(message_offset);
    (fbb.finished_data().to_vec(), body)
}

/// Encode one [`ColumnBatch`] as a complete, self-contained Arrow IPC
/// stream: a Schema message, one RecordBatch message, and the
/// end-of-stream marker. `names` must have one entry per column.
pub fn encode_stream(batch: &ColumnBatch, names: &[&str]) -> Result<Vec<u8>> {
    if names.len() != batch.width() {
        bail!(
            "Arrow export: {} column names for a batch of width {}",
            names.len(),
            batch.width()
        );
    }
    let planned: Vec<PlannedColumn> = (0..batch.width())
        .map(|i| plan_column(batch.column(i)))
        .collect::<Result<_>>()?;

    let mut out = Vec::new();
    let schema_fields: Vec<(&str, &PlannedColumn)> =
        names.iter().copied().zip(planned.iter()).collect();
    let schema_msg = write_schema_message(&schema_fields);
    frame_message(&schema_msg, &[], &mut out);

    let (rb_msg, body) = write_record_batch_message(batch.height(), &planned);
    frame_message(&rb_msg, &body, &mut out);

    write_eos(&mut out);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Num;

    fn v_int(i: i64) -> DataValue {
        DataValue::Num(Num::int(i))
    }

    #[test]
    fn align8_rounds_up_to_the_next_multiple_of_eight() {
        for (len, expected) in [(0, 0), (1, 8), (7, 8), (8, 8), (9, 16), (16, 16)] {
            assert_eq!(align8(len), expected, "len={len}");
        }
    }

    #[test]
    fn validity_bitmap_omits_the_buffer_when_every_row_is_valid() {
        let (null_count, buf) = validity_bitmap(&[true, true, true]);
        assert_eq!(null_count, 0);
        assert!(buf.is_none());
    }

    #[test]
    fn validity_bitmap_marks_lsb_first_bits() {
        // Row 0 null, rows 1-2 valid, row 3 null, rows 4-7 valid: byte 0
        // should have bits 1,2,4,5,6,7 set (LSB = row 0).
        let (null_count, buf) =
            validity_bitmap(&[false, true, true, false, true, true, true, true]);
        assert_eq!(null_count, 2);
        let buf = buf.unwrap();
        assert_eq!(buf.len(), 1);
        assert_eq!(buf[0], 0b1111_0110);
    }

    #[test]
    fn offsets_and_values_starts_at_zero_and_is_monotone() {
        let items: Vec<&[u8]> = vec![b"ab", b"", b"cde"];
        let (offsets, values) = offsets_and_values(items.into_iter()).unwrap();
        let off_i32: Vec<i32> = offsets
            .chunks_exact(4)
            .map(|c| i32::from_le_bytes(c.try_into().unwrap()))
            .collect();
        assert_eq!(off_i32, vec![0, 2, 2, 5]);
        assert_eq!(values, b"abcde");
    }

    #[test]
    fn plan_mixed_column_refuses_genuinely_heterogeneous_kinds() {
        let values = vec![v_int(1), DataValue::Str("x".into())];
        let err = plan_mixed_column(&values).unwrap_err();
        assert!(err.to_string().contains("more than one non-null kind"));
    }

    #[test]
    fn plan_mixed_column_refuses_unmapped_kinds() {
        let values = vec![DataValue::List(vec![v_int(1)])];
        let err = plan_mixed_column(&values).unwrap_err();
        assert!(err.to_string().contains("no Arrow mapping"));
    }

    #[test]
    fn plan_mixed_column_accepts_a_nullable_int_column() {
        let values = vec![v_int(1), DataValue::Null, v_int(3)];
        let planned = plan_mixed_column(&values).unwrap();
        assert_eq!(planned.arrow_type, TYPE_INT);
        assert_eq!(planned.nullability, ArrowNullability::Optional);
        assert_eq!(planned.null_count, 1);
    }

    /// A minimal, real stream: one Int64 column, three rows, encodes
    /// without error and produces bytes shaped like a stream (continuation
    /// marker to start, non-zero length, ends in the EOS marker).
    #[test]
    fn encode_stream_produces_a_framed_byte_sequence() {
        let batch = ColumnBatch::from_rows(
            vec![
                Tuple::from_vec(vec![v_int(1)]),
                Tuple::from_vec(vec![v_int(2)]),
                Tuple::from_vec(vec![v_int(3)]),
            ],
            1,
        );
        let bytes = encode_stream(&batch, &["n"]).unwrap();
        assert!(bytes.len() > 16);
        assert_eq!(&bytes[0..4], &CONTINUATION_MARKER.to_le_bytes());
        assert_eq!(
            &bytes[bytes.len() - 8..bytes.len() - 4],
            &CONTINUATION_MARKER.to_le_bytes()
        );
        assert_eq!(&bytes[bytes.len() - 4..], &0u32.to_le_bytes());
    }

    #[test]
    fn encode_stream_refuses_a_name_count_mismatch() {
        let batch = ColumnBatch::from_rows(vec![Tuple::from_vec(vec![v_int(1)])], 1);
        let err = encode_stream(&batch, &[]).unwrap_err();
        assert!(err.to_string().contains("column names"));
    }
}
