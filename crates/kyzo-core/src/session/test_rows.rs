/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Session-test row assertions — ONE seat for `NamedRows` → `i64` vectors.
//!
//! Every session suite that asserted query results as int matrices used to
//! paste the same map/sort body. That was a second authority by copy-paste
//! (copy_detector). Callers import from here; they do not re-own the decode.

use crate::data::json::NamedRows;
use miette::{Result, miette};

/// Result rows as `i64` vectors preserving query order.
pub(crate) fn raw_int_rows(nr: &NamedRows) -> Result<Vec<Vec<i64>>> {
    nr.rows()
        .iter()
        .map(|r| {
            r.iter()
                .map(|v| v.get_int().ok_or_else(|| miette!("int")))
                .collect::<Result<Vec<_>, _>>()
        })
        .collect()
}

/// Result rows as sorted `i64` vectors, for order-independent assertions.
pub(crate) fn int_rows(nr: &NamedRows) -> Result<Vec<Vec<i64>>> {
    let mut out = raw_int_rows(nr)?;
    out.sort();
    Ok(out)
}
