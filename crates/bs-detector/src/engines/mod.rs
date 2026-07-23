/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The four detection surfaces. Exactly four — this mirrors the ontology of
//! how a claim/reality mismatch becomes visible, not a historical file
//! count:
//! - [`shape`]: the lie is a construct at one site in source text.
//! - [`graph`]: the lie is relational — no single site is wrong.
//! - [`behavior`]: the lie only exists when executed.
//! - [`meta`]: the lie is in the detector itself.

pub mod graph;
pub mod meta;
pub mod shape;

/// One detector hit, before waiver filtering: where and what.
#[derive(Clone, Debug)]
pub struct Hit {
    pub file: String,
    pub line: usize,
    /// The construct name as waivers must swear to it.
    pub construct: String,
}
