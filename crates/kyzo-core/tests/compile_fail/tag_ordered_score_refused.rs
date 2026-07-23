/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Compile-fail: metric scores cannot sit on a TagOrdered path (§14).
//! TagOrdered is Unconstructible — there is no such type to name.

fn main() {
    // Diverging placeholder: the fixture must fail at the type name
    // itself, so no value is ever produced here.
    let _score: kyzo::TagOrdered = loop {};
}
