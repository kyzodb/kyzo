/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The one-law encoding battery (laws, not scenarios).
//!
//! Re-homed from condemned `storage/tests.rs` encoding section.
//!
//! - **Law 1 (round-trip)**: decode(encode(v)) == v
//! - **Law 2 (order embedding)**: encode(a) cmp encode(b) == a cmp b — exhaustive pairwise
//! - **Law 3 (no panic on corrupt)**: decode arbitrary / byte-flipped bytes never panics
//! - **Expr codec (seat 59)**: serde round-trip under one normative encoding,
//!   pinned by a golden Binding vector (and a multi-variant tree).
//!
//! DEVIATIONS note: `kyzo-model/src/lib.rs` is not on this task's allowlist, so
//! `pub mod format` is not wired at the crate root yet — path meters still hold.
//! The Expr golden is also compile-checked under `program/expr.rs` tests until
//! that door opens.

use crate::SourceSpan;
use crate::program::expr::{BindingPos, Expr, LazyOp};
use crate::program::op;
use crate::program::symbol::Symbol;
use miette::{IntoDiagnostic, Result, miette};
use crate::value::{
    Bound, DataValue, Interval, Num, RegexFlags, RegexSource, UuidWrapper, ValiditySlot,
    ValidityTs, Vector, append_canonical,
};

fn corpus() -> Result<Vec<DataValue>> {
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
        DataValue::Num(Num::float(1.0)),
        DataValue::Num(Num::float(1.5)),
        DataValue::Num(Num::int(2)),
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
        DataValue::Uuid(UuidWrapper::new(uuid::Uuid::from_u128(0))),
        DataValue::Uuid(UuidWrapper::new(uuid::Uuid::from_u128(
            0x1234_5678_9abc_def0_1234_5678_9abc_def0,
        ))),
        DataValue::Regex(RegexSource::validated(RegexFlags::NONE, "^a.*b$".into()).into_diagnostic()?),
        DataValue::Regex(RegexSource::validated(RegexFlags::NONE, "x+".into()).into_diagnostic()?),
        DataValue::Vector(Vector::try_new(vec![0.0, -0.0, 1.0]).ok_or_else(|| miette!("vector"))?),
        DataValue::Vector(Vector::try_new(vec![-1.5, 2.5]).ok_or_else(|| miette!("vector"))?),
        DataValue::Validity(ValiditySlot::from_stored(ValidityTs::from_raw(0), true)),
        DataValue::Validity(ValiditySlot::from_stored(ValidityTs::from_raw(1), false)),
        DataValue::Interval(Interval::new(Bound::Closed(0), Bound::Closed(1))),
        DataValue::Interval(Interval::new(Bound::Closed(-1), Bound::Closed(-1))),
        DataValue::List(vec![]),
        DataValue::List(vec![DataValue::Num(Num::int(1))]),
        DataValue::Set([DataValue::Num(Num::int(1))].into_iter().collect()),
    ];
    let nested_set = DataValue::Set(
        [DataValue::Num(Num::int(1)), DataValue::Num(Num::int(2))]
            .into_iter()
            .collect(),
    );
    let nested_list = DataValue::List(vec![DataValue::Num(Num::int(1))]);
    c.push(DataValue::List(vec![nested_set, nested_list]));
    Ok(c)
}

fn encode(v: &DataValue) -> Vec<u8> {
    let mut buf = vec![];
    append_canonical(&mut buf, v);
    buf
}

#[test]
fn law1_round_trip_corpus() -> Result<()> {
    for v in corpus()? {
        let buf = encode(&v);
        let (decoded, rest) = match DataValue::decode_from_key(&buf) {
            Ok(pair) => pair,
            Err(e) => {
                assert!(false, "decode failed for {v:?}: {e}");
                return Ok(());
            }
        };
        assert_eq!(decoded, v, "round-trip failed for {v:?}");
        assert!(rest.is_empty(), "trailing bytes for {v:?}");
    }
    Ok(())
}

/// Exhaustive PAIRWISE check: cross-type disagreements cannot hide behind
/// sort stability, and a failure names the exact offending pair.
#[test]
fn law2_order_embedding_corpus_pairwise() -> Result<()> {
    let values = corpus()?;
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
    Ok(())
}

/// Deterministic corruption harness: every single-byte mutation of every
/// corpus encoding must decode to an error or a value — never a panic.
#[test]
fn law3_byte_flip_harness() -> Result<()> {
    for v in corpus()? {
        let buf = encode(&v);
        for i in 0..buf.len() {
            for flip in [0x01u8, 0x80, 0xFF] {
                let mut m = buf.clone();
                m[i] ^= flip;
                match DataValue::decode_from_key(&m) {
                    Ok(v) => core::mem::drop(v),
                    Err(e) => core::mem::drop(e),
                }
            }
        }
    }
    Ok(())
}

#[test]
fn law_vector_signed_zero_canonicalizes() -> Result<()> {
    let a = DataValue::Vector(Vector::try_new(vec![-0.0]).ok_or_else(|| miette!("vector"))?);
    let b = DataValue::Vector(Vector::try_new(vec![0.0]).ok_or_else(|| miette!("vector"))?);
    assert_eq!(encode(&a), encode(&b), "signed zero must canonicalize");
    Ok(())
}

#[test]
fn law_scalar_num_negative_zero_collapses_to_positive() {
    let a = DataValue::Num(Num::float(-0.0));
    let b = DataValue::Num(Num::float(0.0));
    assert_eq!(encode(&a), encode(&b));
}

/// Permanent Binding wire form — seat 59 / story #352 T1 golden vector.
const EXPR_BINDING_GOLDEN: &str = r#"{"Binding":{"var":{"name":"x"},"tuple_pos":"Unresolved"}}"#;

/// Expr under one complete canonical serde codec, both directions:
/// encode → decode identity, and the Binding golden bytes stay put.
#[test]
fn expr_canonical_round_trip_golden() -> Result<()> {
    let binding: Expr = serde_json::from_str(EXPR_BINDING_GOLDEN).into_diagnostic()?;
    assert_eq!(
        serde_json::to_string(&binding).into_diagnostic()?,
        EXPR_BINDING_GOLDEN,
        "Binding golden vector moved"
    );

    let span = SourceSpan::default();
    let tree = Expr::Lazy {
        op: LazyOp::And,
        args: Box::new([
            Expr::Apply {
                op: op::OP_EQ,
                args: Box::new([
                    Expr::Binding {
                        var: Symbol::new("a", span),
                        tuple_pos: BindingPos::Resolved(0),
                    },
                    Expr::Const {
                        val: DataValue::from(1i64),
                        span,
                    },
                ]),
                span,
            },
            Expr::Cond {
                clauses: vec![(
                    Expr::Const {
                        val: DataValue::from(true),
                        span,
                    },
                    Expr::UnboundApply {
                        op: "custom".into(),
                        args: Box::new([Expr::Const {
                            val: DataValue::Null,
                            span,
                        }]),
                        span,
                    },
                )],
                span,
            },
        ]),
        span,
    };
    let bytes = serde_json::to_vec(&tree).into_diagnostic()?;
    let back: Expr = serde_json::from_slice(&bytes).into_diagnostic()?;
    assert_eq!(back, tree, "Expr tree round-trip changed identity");
    Ok(())
}
