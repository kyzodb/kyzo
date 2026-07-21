/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Compile-fail: BoundOp is not constructible at the public door.
//! Minting is sealed inside bind_op; external struct literals are refused.

fn main() {
    let _ = kyzo::BoundOp {
        decl: kyzo_model::program::op::OP_ADD,
        body: (|_| unimplemented!()) as fn(&[kyzo_model::DataValue]) -> miette::Result<kyzo_model::DataValue>,
    };
}
