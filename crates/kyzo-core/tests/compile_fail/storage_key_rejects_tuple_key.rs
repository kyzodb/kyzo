/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Compile-fail: a bare TupleKey is not a StorageKey — cross-use refuses.
use kyzo::{StorageKey, TupleKey};

fn needs_storage(_k: StorageKey) {}

fn main() {
    let bare = TupleKey::from_values(&[]);
    needs_storage(bare);
}
