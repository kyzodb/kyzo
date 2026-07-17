/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Trybuild harness: build→seal→query machine absences (story #305 T2).
//! Query on the builder and query on a stale projection are type errors.

#[test]
fn projection_query_on_builder_refused() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail/projection_query_on_builder.rs");
}

#[test]
fn projection_query_on_stale_refused() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail/projection_query_on_stale.rs");
}
