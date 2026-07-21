/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! temporal_format.rs — stdlib kernel (move_plan).

use jiff::tz::{Offset, TimeZone};
use miette::{Result, miette};

use kyzo_model::data_value_any;
use kyzo_model::value::DataValue;

use crate::exec::stdlib::errors::TimestampFormatRefused;

pub(crate) fn op_format_timestamp(args: &[DataValue]) -> Result<DataValue> {
    let millis = match &args[0] {
        DataValue::Validity(vld) => vld.ts_micros() / 1000,
        v @ (data_value_any!()) => {
            let f = v
                .get_float()
                .ok_or_else(|| miette!("'format_timestamp' expects a number"))?;
            (f * 1000.) as i64
        }
    };
    let ts =
        jiff::Timestamp::from_millisecond(millis).map_err(|_| miette!("bad time: {}", &args[0]))?;
    let off = match args.get(1) {
        Some(tz_v) => {
            let tz_s = tz_v.get_str().ok_or_else(|| {
                miette!("'format_timestamp' timezone specification requires a string")
            })?;
            let tz =
                TimeZone::get(tz_s).map_err(|_| miette!("bad timezone specification: {}", tz_s))?;
            tz.to_offset(ts)
        }
        None => Offset::UTC,
    };
    Ok(DataValue::Str(format_rfc3339(ts, off)?))
}

/// Parses an RFC 3339 / ISO 8601 timestamp string to seconds since the Unix
/// epoch (a float; pre-1970 inputs are negative). A `:60` leap second is
/// clamped to `:59` of the same minute — jiff does not fold it into the
/// following second, which the former chrono implementation did, so an input
/// like `2016-12-31T23:59:60Z` parses one second earlier than it used to.
pub(crate) fn op_parse_timestamp(args: &[DataValue]) -> Result<DataValue> {
    let s = args[0]
        .get_str()
        .ok_or_else(|| miette!("'parse_timestamp' expects a string"))?;
    let ts: jiff::Timestamp = s.parse().map_err(|_| miette!("bad datetime: {}", s))?;
    // Pre-epoch datetimes yield negative seconds. Decomposed as chrono did —
    // a FLOORED whole second plus a non-negative subsecond nanosecond count —
    // so the lossy f64 result is bit-identical across the epoch boundary.
    let nanos = ts.as_nanosecond();
    let secs =
        nanos.div_euclid(1_000_000_000) as f64 + (nanos.rem_euclid(1_000_000_000) as f64) / 1e9;
    Ok(DataValue::from(secs))
}

fn autosi_precision(subsec_nanos: i32) -> Option<u8> {
    let n = subsec_nanos.unsigned_abs();
    if n == 0 {
        None
    } else if n.is_multiple_of(1_000_000) {
        Some(3)
    } else if n.is_multiple_of(1_000) {
        Some(6)
    } else {
        Some(9)
    }
}

fn format_rfc3339(ts: jiff::Timestamp, off: Offset) -> Result<String> {
    let prec = autosi_precision(ts.subsec_nanosecond());
    let zoned = ts.to_zoned(TimeZone::fixed(off));
    let mut buf = String::new();
    jiff::fmt::temporal::DateTimePrinter::new()
        .precision(prec)
        .print_zoned(&zoned, &mut buf)
        .map_err(|_| TimestampFormatRefused)?;
    if let Some(i) = buf.rfind('[') {
        buf.truncate(i);
    }
    Ok(buf)
}
