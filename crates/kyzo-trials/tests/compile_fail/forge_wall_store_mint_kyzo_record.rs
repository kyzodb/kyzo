/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Seat 8: store-shaped mint of KyzoRecord is Unconstructible at the public door.
//! Private constructors live only at session admission (`admit_record`).

fn main() {
    let _ = kyzo_trials::KyzoRecord {
        core: (),
    };
}
