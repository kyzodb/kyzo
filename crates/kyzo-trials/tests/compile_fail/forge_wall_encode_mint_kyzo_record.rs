/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Seat 8: encode / put currency cannot mint KyzoRecord.

use kyzo_trials::WriteTx;

#[allow(dead_code)]
fn forge_from_put_bytes<T: WriteTx>(tx: &mut T, key: &[u8], val: &[u8]) {
    let _ = tx.put(key, val);
    let _ = kyzo_trials::KyzoRecord::from_store_bytes(val);
}

fn main() {}
