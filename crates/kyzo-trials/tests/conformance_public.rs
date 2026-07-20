/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Out-of-crate §85 proof: the conformance battery is a public surface.
//!
//! This integration target lives outside `kyzo-trials` source and would fail
//! to compile if `run_full_battery` / `law_*` were not `pub`.

use kyzo::new_fjall_storage;
use kyzo_trials::{law_send_sync_bounds_are_compiler_checked, run_full_battery};

#[test]
fn public_battery_invoked_from_integration_target() {
    law_send_sync_bounds_are_compiler_checked::<kyzo::FjallStorage>();
    run_full_battery(|| {
        // Each law wants a fresh, empty store; a fjall store's live
        // file handles keep working after its directory is unlinked
        // from the tree, so leaking the `TempDir` guard (rather than
        // threading a handle through every law) is the right call for
        // test scaffolding that must hand back a bare `S`.
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        std::mem::forget(dir);
        db
    });
}
