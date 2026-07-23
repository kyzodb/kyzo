/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Projection zone: rebuildable indexes derived from stored facts.

pub(crate) mod contract;
pub(crate) mod current;
pub(crate) mod dimension;
pub(crate) mod projection;
pub(crate) mod residency;
#[cfg(test)]
pub(crate) mod retrieval;

#[cfg(test)]
pub(crate) mod index_fixture;

#[cfg(test)]
pub(crate) mod gazetteer;

pub(crate) mod text;

pub(crate) mod dedup;
#[cfg(test)]
pub(crate) mod sparse;
// Spatial seats complete; IndexKind::Spatial host [OPEN] → cfg(test) until host.
#[cfg(test)]
pub(crate) mod spatial;
pub(crate) mod vector;
