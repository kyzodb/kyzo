/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
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
    let micros_i128 = ts.as_nanosecond().div_euclid(1000);
    match i64::try_from(micros_i128) {
        Ok(us) => us,
        // jiff::Timestamp's civil range is inside i64 microseconds; this arm
        // is the total door for an out-of-range i128 quotient without `as`.
        Err(_overflow) if micros_i128 < 0 => i64::MIN,
        Err(_overflow) => i64::MAX,
    }
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
        _other => bail!(BadValiditySpecification(span)),
    }
}

#[cfg(test)]
mod tests {
    use miette::{IntoDiagnostic, Result, miette};

    use super::*;
    use crate::program::span::SourceSpan;
    use crate::value::{DataValue, ValidityTs};

    #[test]
    fn i64_max_numeric_passthrough_is_end_coordinate() -> Result<()> {
        // Coerce is not the write-assertion door: i64::MAX is a lawful
        // seek/spec coordinate (same as "END"). Refusal of assert+MAX lives
        // on ValidityTs::for_assertion / Validity::new — pinned here so a
        // future "tighten coerce" PR cannot silently shrink the law.
        let span = SourceSpan(0, 0);
        let cur = ValidityTs::from_raw(1);
        let got = data_value_to_vld_spec(DataValue::from(i64::MAX), span, cur).into_diagnostic()?;
        assert_eq!(got, MAX_VALIDITY_TS);
        assert_eq!(got.raw(), i64::MAX);
        // The write door still refuses the reserved terminal tick.
        assert!(ValidityTs::for_assertion(i64::MAX).is_none());
        Ok(())
    }

    #[test]
    fn now_and_end_strings_and_finite_micros() -> Result<()> {
        let span = SourceSpan(0, 0);
        let cur = ValidityTs::from_raw(42);
        assert_eq!(
            data_value_to_vld_spec(DataValue::from("NOW"), span, cur).into_diagnostic()?,
            cur
        );
        assert_eq!(
            data_value_to_vld_spec(DataValue::from("END"), span, cur).into_diagnostic()?,
            MAX_VALIDITY_TS
        );
        assert_eq!(
            data_value_to_vld_spec(DataValue::from(99_i64), span, cur)
                .into_diagnostic()?
                .raw(),
            99
        );
        Ok(())
    }
}
