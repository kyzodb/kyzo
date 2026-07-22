/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Geospatial index projections.
//!
//! Search door: [`RelationIndexSearch`](crate::project::projection::RelationIndexSearch)
//! on [`index::Spatial`]. Session `IndexKind::Spatial` create/mutation arm is
//! [OPEN] — algorithm seats are complete; this module is `#[cfg(test)]` until
//! that host arm lands (no module-level dead_code allow).

pub(crate) mod index;
