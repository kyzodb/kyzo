/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Thread-count determinism lane (campaigns_proposed).
//!
//! Capability 1's `determinism_campaign` / `run_seed` in [`crate::gauntlet`]
//! is the living campaign: generated programs at 1/2/4/8 rayon threads with
//! byte-identical answers, witness tables, and refusals.
//!
//! This module is the named seat for that lane — points at Cap1 so board /
//! architecture references to `kyzo-trials/determinism.rs` resolve without
//! a second corpus.

#![cfg(test)]

/// Point at Cap1's campaign (defined in [`crate::gauntlet`]).
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn thread_count_determinism_lane_shares_cap1() {
    // Smoke: one seed through the shared Cap1 door. The full sweep lives
    // as `gauntlet::determinism_campaign` (KYZO_TRIALS_SEEDS / BASE).
    crate::gauntlet::run_seed(0).expect("Cap1 run_seed holds for seed 0");
}
