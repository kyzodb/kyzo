/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Compile-fail proof: [`Stale`] has no query surface.
//! A generation mismatch is the distinguishable [`Stale`] type — querying
//! it is a type error, not an `Option`/`Err` from a get-shaped call
//! (story #305).

use kyzo::{Generation, ProjectionBuilder, ProjectionKind, Stale};

struct DemoKind;

impl ProjectionKind for DemoKind {
    type Query = ();
    type Candidates = ();

    fn search(&self, _query: &Self::Query) -> Self::Candidates {}
}

fn _query_on_stale(stale: Stale<DemoKind>) {
    let refused_query_on_stale = stale.query(&());
}

fn _classify_path_yields_stale() -> Stale<DemoKind> {
    let sealed = ProjectionBuilder::new(DemoKind).seal(Generation::new(1));
    Generation::new(2)
        .classify(sealed)
        .expect_err("mismatched generation is Stale")
}

fn main() {}
