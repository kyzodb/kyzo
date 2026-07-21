/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Sparse vector index projections.

#[allow(clippy::module_inception)] // zone folder + impl module share the seat name by design
pub(crate) mod sparse;
#[cfg(test)]
pub(crate) mod sparse_hostile;
