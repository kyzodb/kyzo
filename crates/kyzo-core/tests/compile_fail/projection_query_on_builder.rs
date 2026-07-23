/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Compile-fail proof: [`ProjectionBuilder`] has no query surface.
//! Querying an unsealed projection is absent from the interface, not a
//! runtime error a method returns (story #305).

use kyzo::{ProjectionBuilder, ProjectionKind};

struct DemoKind;

impl ProjectionKind for DemoKind {
    type Query = ();
    type Candidates = ();

    fn search(&self, _query: &Self::Query) -> Self::Candidates {}
}

fn _query_on_builder(builder: ProjectionBuilder<DemoKind>) {
    let refused_query_on_builder = builder.query(&());
}

fn main() {}
