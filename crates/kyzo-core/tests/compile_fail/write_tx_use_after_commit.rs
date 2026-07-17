/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Compile-fail proof: `commit(self)` spends Open — `put` on the stale
//! binding is a type error (use-after-commit cannot compile).

use kyzo::WriteTx;

#[allow(dead_code)]
fn spent_after_commit<T: WriteTx>(mut tx: T) {
    let _ = tx.commit();
    let _ = tx.put(b"k", b"v");
}

fn main() {}
