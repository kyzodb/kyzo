/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Compile-fail proof: zero arity is unrepresentable.
//!
//! [`Arity::new`] requires a `NonZeroUsize`. Passing a bare `0` is a type
//! error — zero width cannot be smuggled past the door (story #306 T3).

use kyzo::Arity;

fn main() {
    let refused_zero_arity = Arity::new(0);
}
