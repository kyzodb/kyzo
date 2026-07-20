/*
 * Copyright 2026, The KyzoDB Authors.
 * MPL-2.0.
 */

//! One `@` / write-coordinate validity coercion law shared by parse and mutate.
//!
//! Integer microseconds, `"NOW"` / `"END"`, or RFC 3339 — never redefined in
//! exec kernels or parse call sites.

use miette::{Diagnostic, Result, bail};
use thiserror::Error;

use crate::program::span::SourceSpan;
use crate::value::{DataValue, MAX_VALIDITY_TS, ValidityTs};

/// Floored microseconds since Unix epoch (toward −∞). Shared by validity
/// coercion and timestamp parse so both agree on the containing microsecond.
pub fn timestamp_to_micros(ts: jiff::Timestamp) -> i64 {
    (ts.as_nanosecond().div_euclid(1000)) as i64
}

/// Parses an RFC 3339 / ISO 8601 timestamp string to a validity timestamp in
/// microseconds since the Unix epoch, floored toward negative infinity.
pub fn str2vld(s: &str) -> Result<ValidityTs> {
    let ts: jiff::Timestamp = s
        .parse()
        .map_err(|_| miette::miette!("bad datetime: {}", s))?;
    Ok(ValidityTs::from_raw(timestamp_to_micros(ts)))
}

#[derive(Debug, Error, Diagnostic)]
#[error("bad specification of validity")]
#[diagnostic(code(parser::bad_validity_spec))]
pub struct BadValiditySpecification(#[label] pub SourceSpan);

/// Interpret an already-evaluated [`DataValue`] as a validity coordinate.
pub fn data_value_to_vld_spec(
    val: DataValue,
    span: SourceSpan,
    cur_vld: ValidityTs,
) -> Result<ValidityTs> {
    match val {
        DataValue::Num(n) => {
            let microseconds = n.as_int().ok_or(BadValiditySpecification(span))?;
            // Coerce only: any representable instant is a valid coordinate.
            // The write-assertion door (`ValidityTs::for_assertion`) refuses
            // the reserved terminal tick at the mutation boundary — not here.
            Ok(ValidityTs::from_raw(microseconds))
        }
        DataValue::Str(s) => match &s as &str {
            "NOW" => Ok(cur_vld),
            "END" => Ok(MAX_VALIDITY_TS),
            s => Ok(str2vld(s).map_err(|_| BadValiditySpecification(span))?),
        },
        _ => bail!(BadValiditySpecification(span)),
    }
}
