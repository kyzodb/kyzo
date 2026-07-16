/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Compile-fail proof: typed [`CommitFailure`] has no `downcast_ref` —
//! conflict / IO / corruption are closed enum variants; first-party code
//! matches the enum, it does not recover identity through an erased carrier.

use kyzo::{CommitFailure, ConflictError};

#[allow(dead_code)]
fn branch_by_downcast(err: CommitFailure) {
    let _ = err.downcast_ref::<ConflictError>();
}

fn main() {}
