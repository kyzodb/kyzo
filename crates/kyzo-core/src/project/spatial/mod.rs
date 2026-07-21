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
//! on [`spatial::Spatial`]. Session `IndexKind::Spatial` create/mutation arm is
//! [OPEN] — the algorithm seats here are complete; the allow covers surfaces
//! waiting on that host arm (not mid-wiring placeholders).
#![allow(dead_code)] // [OPEN] session IndexKind::Spatial host arm

#[allow(clippy::module_inception)] // zone folder + impl module share the seat name by design
pub(crate) mod spatial;
