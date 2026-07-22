/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Re-homed domain tables from data/tests/functions.rs.
use miette::{Result, miette};
use crate::exec::stdlib::temporal_format::*;
use kyzo_model::data_value_any;
use kyzo_model::schema::{ColType, NullableColType};
use kyzo_model::str2vld;
use kyzo_model::value::{DataValue, ValidityTs};


#[test]
fn test_pre_epoch_timestamps() -> Result<()>  {
    let secs = op_parse_timestamp(&[DataValue::from("1969-07-20T20:17:00Z")])?
        .get_float()
        ?;
    assert!(secs < 0.);

    let vld = str2vld("1969-07-20T20:17:00Z")?;
    assert!(vld.raw() < 0);

    // The schema boundary obeys the same law: coercing a pre-epoch validity
    // string yields negative microseconds.
    let typing = NullableColType::required(ColType::Validity);
    let coerced = typing
        .coerce(
            DataValue::Str("1969-07-20T20:17:00Z".into()),
            ValidityTs::from_raw(0),
        )
        ?;
    match coerced {
        DataValue::Validity(vld) => assert!(vld.timestamp().raw() < 0),
        v @ (data_value_any!()) => panic!("expected a validity, got {v:?}"),
    }
    Ok(())
}
