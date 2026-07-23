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

fn refused_bound_op_body(
    _: &[kyzo_model::DataValue],
) -> miette::Result<kyzo_model::DataValue> {
    // Diverging placeholder: this fixture must fail at the struct literal
    // below, so this body can never run.
    loop {}
}

fn main() {
    let refused_bound_op_mint = kyzo::BoundOp {
        decl: kyzo_model::program::op::OP_ADD,
        // Function-item coerces to fn-pointer — no `as` cast costume.
        body: refused_bound_op_body,
    };
}
