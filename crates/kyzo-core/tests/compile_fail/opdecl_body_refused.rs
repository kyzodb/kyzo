/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Compile-fail: OpDecl cannot carry a callable body (Unconstructible).
use kyzo_model::OpDecl;

fn main() {
    let _ = OpDecl {
        name: "OP_ADD",
        min_arity: 0,
        vararg: true,
        deterministic: true,
        body: 0u8,
    };
}
