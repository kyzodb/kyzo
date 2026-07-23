/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The session zone: live handles, admission, catalog — not evaluation itself.
//!
//! Everything between a caller and the query/storage organs: the [`db`]
//! entrypoint, the mutation tier ([`admit`]), index lifecycle ([`ops`]), the
//! catalog, transaction-scoped [`constraint`]s, and change [`observe`]rs.

/// The engine's ONE wall-clock read: the system-time stamp for writes.
/// Lives in the session tier by law — the value plane has no ambient
/// clock, and determinism campaigns replay stamps rather than minting
/// them.
pub fn current_validity() -> miette::Result<kyzo_model::value::ValidityTs> {
    let micros = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| miette::miette!("system clock before the epoch: {e}"))?
        .as_micros();
    let micros: i64 = micros
        .try_into()
        .map_err(|_| miette::miette!("system clock beyond i64 microseconds"))?;
    Ok(kyzo_model::value::ValidityTs::of_micros(micros))
}

pub(crate) mod access;
pub(crate) mod admit;
pub(crate) mod capacity;
pub(crate) mod catalog;
pub(crate) mod certificate_inject;
pub(crate) mod composition;
pub(crate) mod constraint;
pub(crate) mod db;
pub(crate) mod footprint;
pub(crate) mod fts;
pub(crate) mod generation;
pub(crate) mod hnsw;
pub(crate) mod jobs;
pub(crate) mod json;
pub(crate) mod lsh;
pub(crate) mod normalize;
pub(crate) mod observe;
pub(crate) mod ops;
pub(crate) mod record_id;
#[cfg(test)]
pub(crate) mod test_rows;
pub(crate) mod verify;
