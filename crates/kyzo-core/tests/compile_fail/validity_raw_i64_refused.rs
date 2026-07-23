/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Compile-fail proof: `Validity::new(i64::MAX, true)` is unrepresentable.
//!
//! `Validity::new` requires a `ValidityTs` as its first argument. Passing a
//! bare `i64` is a type error — the reserved tick (`i64::MAX`) cannot be
//! smuggled into a `Validity` through this call, because `i64` is not
//! `ValidityTs`. The only user-assertion door (`ValidityTs::for_assertion`)
//! already refuses `i64::MAX`. This file must not compile.

use kyzo::Validity;

fn main() {
    let refused_raw_max_validity = Validity::new(i64::MAX, true);
}
