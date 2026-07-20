/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Seat 8: WAL / SST decode must not type as a KyzoRecord mint.

fn main() {
    let _ = kyzo_trials::KyzoRecord::from_wal_bytes(b"checksum-valid");
    let _ = kyzo_trials::KyzoRecord::from_sst_row(b"relocated-sst");
}
