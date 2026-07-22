/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Interval kernel domain tables — six boundary primitives + intersects; EMPTY lawful.
use miette::{Result, miette};
use crate::exec::stdlib::interval::*;
use crate::exec::stdlib::resolve_op;
use kyzo_model::value::{Bound, DataValue, Interval};

fn iv(lo: i64, hi: i64) -> DataValue {
    DataValue::Interval(Interval::new(Bound::Closed(lo), Bound::Closed(hi)))
}

#[test]
fn make_interval_and_bounds() -> Result<()>  {
    let make = resolve_op("make_interval").ok_or_else(|| miette!("resolve_op"))?;
    let v = make
        .apply(&[DataValue::from(1i64), DataValue::from(4i64)])?;
    let start = resolve_op("interval_start").ok_or_else(|| miette!("resolve_op"))?;
    let end = resolve_op("interval_end").ok_or_else(|| miette!("resolve_op"))?;
    assert_eq!(
        start.apply(std::slice::from_ref(&v))?,
        DataValue::from(1i64)
    );
    assert_eq!(end.apply(&[v])?, DataValue::from(4i64));
    Ok(())
}

#[test]
fn has_start_has_end_unbounded_flags() -> Result<()>  {
    let open = DataValue::Interval(Interval::new(Bound::Unbounded, Bound::Closed(5)));
    assert_eq!(
        op_interval_has_start(std::slice::from_ref(&open))?,
        DataValue::from(false)
    );
    assert_eq!(
        op_interval_has_end(std::slice::from_ref(&open))?,
        DataValue::from(true)
    );
    assert_eq!(
        op_interval_is_start_unbounded(std::slice::from_ref(&open))?,
        DataValue::from(true)
    );
    assert_eq!(
        op_interval_is_end_unbounded(&[open])?,
        DataValue::from(false)
    );
    Ok(())
}

#[test]
fn intersects_and_empty_start_gt_end_lawful() -> Result<()>  {
    let a = iv(1, 3);
    let b = iv(2, 5);
    assert_eq!(
        op_interval_intersects(&[a.clone(), b])?,
        DataValue::from(true)
    );
    // EMPTY: start > end is a lawful empty interval, not a panic.
    let empty = DataValue::Interval(Interval::EMPTY);
    assert_eq!(
        op_interval_has_start(std::slice::from_ref(&empty))?,
        DataValue::from(false)
    );
    assert_eq!(
        op_interval_has_end(&[empty])?,
        DataValue::from(false)
    );
    Ok(())
}
