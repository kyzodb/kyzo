/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! ONE seat for HTTP payload map decode: required string field + optional `params`.
//!
//! `query::QueryPayload` and `feeds::StandingQueryParams` used to paste the
//! same visit_map body (copy_detector). Callers supply the required key name
//! and the absent-params default; they do not re-own the map walk.

use serde::Deserialize;
use serde::de::{self, MapAccess};

/// Pull `required_key` (string) and optional `params` from a serde map.
/// Unknown fields are skipped. Missing required key is a typed serde error.
pub(super) fn take_required_string_and_params<'de, A, P>(
    mut map: A,
    required_key: &'static str,
) -> Result<(String, Option<P>), A::Error>
where
    A: MapAccess<'de>,
    P: Deserialize<'de>,
{
    let mut required = None;
    let mut params = None;
    while let Some(key) = map.next_key::<String>()? {
        match key.as_str() {
            k if k == required_key => required = Some(map.next_value()?),
            "params" => params = Some(map.next_value()?),
            _unknown_field => {
                let _skipped: de::IgnoredAny = map.next_value()?;
            }
        }
    }
    Ok((
        required.ok_or_else(|| de::Error::missing_field(required_key))?,
        params,
    ))
}
