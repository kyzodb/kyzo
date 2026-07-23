/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! What happens when a check fires: a closed sum of exactly two poles.
//! The baseline is zero by standing operator law — no ratchet/baseline
//! variant may exist here (BANNED #20/#21).

use serde::Deserialize;

/// Firing policy for a registered check.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Policy {
    /// No waiver can exist for this check: every hit is a violation, and a
    /// waivers.toml entry naming this check is itself a run-level error.
    HardBan,
    /// A hit may be confessed by an individually-sworn waiver the operator
    /// audits; unconfessed hits are violations, drifted waivers are
    /// violations, and blanket confessions are not constructible (a waiver
    /// binds to exactly one site).
    SwornWaiver,
}
