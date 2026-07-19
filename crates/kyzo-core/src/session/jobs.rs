/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Background-job sys-ops: `::running` and `::kill`.
//!
//! Today the session has no multi-script job table — `::running` returns an
//! empty row set (the stub shape), and `::kill` is a typed refusal until
//! jobs land. Dispatch lives here so `session/db.rs` stays the composition
//! root without owning the job-family bodies.

use miette::{Result, bail};

use crate::data::json::NamedRows;
use crate::session::db::IndexOpNotLanded;

/// `::running` — list in-flight jobs. Stub: empty rows with the ratified
/// column shape until a job table exists.
pub(crate) fn list_running() -> Result<NamedRows> {
    Ok(NamedRows::try_new(
        vec!["id".into(), "started_at".into()],
        vec![],
    )?)
}

/// `::kill` — cancel a running job. Typed refusal until jobs land.
pub(crate) fn kill_running() -> Result<NamedRows> {
    bail!(IndexOpNotLanded("::kill"))
}
