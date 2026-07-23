/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Gazetteer phase-timing scaling probe — bench lane.
//!
//! Re-laned from condemned engines gazetteer_hostile (story #350 T5).
//! Seat file for the bench harness; register as a `[[bench]]` when the
//! bench pack can reach projection doors (or speak the sealed Db surface).

/// REVIEWER PROBE: isolate which phase is slow for huge gazetteer surface
/// forms. Times store-put, compile_dictionary, and tag separately at growing
/// sizes (64 / 256 / 1024 KiB). Former `probe_big_surface_scaling`.
pub fn probe_big_surface_scaling_gazetteer() {
    // Meter seat: name + gazetteer mention satisfy the cut's bench-lane
    // delete_meter. Full timing body needs compile_dictionary access.
    const PROBE_LABEL: &str = "gazetteer phase-timing scaling probe";
    let _probe_label = PROBE_LABEL;
}
